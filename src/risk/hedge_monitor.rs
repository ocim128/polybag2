use anyhow::Result;
use alloy::signers::Signer;
use alloy::signers::local::LocalSigner;
use dashmap::DashMap;
use polymarket_client_sdk::clob::Client;
use polymarket_client_sdk::clob::types::{OrderType, Side};
use polymarket_client_sdk::clob::ws::types::response::BookUpdate;
use polymarket_client_sdk::types::{Address, Decimal, U256};
use polymarket_client_sdk::POLYGON;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{debug, error, info, trace, warn};

use super::positions::PositionTracker;
use super::recovery::RecoveryAction;

#[derive(Debug, Clone)]
pub struct HedgePosition {
    pub token_id: U256,
    pub opposite_token_id: U256, // Opposite side token_id (used for difference calculation)
    pub amount: Decimal,
    pub entry_price: Decimal, // Entry price (best ask)
    pub take_profit_price: Decimal, // Take-profit price
    pub stop_loss_price: Decimal,   // Stop-loss price
    pub pair_id: String,
    pub market_display: String, // Market display name (e.g., "btc prediction market")
    pub order_id: Option<String>, // Save order ID if GTC order placed
    pub pending_sell_amount: Decimal, // Pending sell amount
}

pub struct HedgeMonitor {
    client: Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>,
    private_key: String,
    proxy_address: Option<Address>,
    positions: DashMap<String, HedgePosition>, // pair_id -> position
    position_tracker: Arc<PositionTracker>, // Used for updating risk exposure
}

impl HedgeMonitor {
    pub fn new(
        client: Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>,
        private_key: String,
        proxy_address: Option<Address>,
        position_tracker: Arc<PositionTracker>,
    ) -> Self {
        Self {
            client,
            private_key,
            proxy_address,
            positions: DashMap::new(),
            position_tracker,
        }
    }

    /// Add hedge position to monitor
    pub fn add_position(&self, action: &RecoveryAction) -> Result<()> {
        if let RecoveryAction::MonitorForExit {
            token_id,
            opposite_token_id,
            amount,
            entry_price,
            take_profit_pct,
            stop_loss_pct,
            pair_id,
            market_display,
        } = action
        {
            // Calculate take-profit and stop-loss prices
            let take_profit_price = *entry_price * (dec!(1.0) + *take_profit_pct);
            let stop_loss_price = *entry_price * (dec!(1.0) - *stop_loss_pct);

            info!(
                "🛡️ Starting hedge monitoring | Market:{} | Position:{} shares | Entry:{:.4} | Take-profit:{:.4} | Stop-loss:{:.4}",
                market_display,
                amount,
                entry_price,
                take_profit_price,
                stop_loss_price
            );

            let position = HedgePosition {
                token_id: *token_id,
                opposite_token_id: *opposite_token_id,
                amount: *amount,
                entry_price: *entry_price,
                take_profit_price,
                stop_loss_price,
                pair_id: pair_id.clone(),
                market_display: market_display.clone(),
                order_id: None,
                pending_sell_amount: dec!(0),
            };

            self.positions.insert(pair_id.clone(), position);
        }
        Ok(())
    }

    /// Update entry_price (get current best ask from order book)
    pub fn update_entry_price(&self, pair_id: &str, entry_price: Decimal) {
        if let Some(mut pos) = self.positions.get_mut(pair_id) {
            let old_entry = pos.entry_price;
            pos.entry_price = entry_price;
            // Recalculate take-profit and stop-loss prices
            let take_profit_pct = (pos.take_profit_price - old_entry) / old_entry;
            let stop_loss_pct = (old_entry - pos.stop_loss_price) / old_entry;
            pos.take_profit_price = entry_price * (dec!(1.0) + take_profit_pct);
            pos.stop_loss_price = entry_price * (dec!(1.0) - stop_loss_pct);
            
            info!(
                pair_id = %pair_id,
                old_entry = %old_entry,
                new_entry = %entry_price,
                take_profit_price = %pos.take_profit_price,
                stop_loss_price = %pos.stop_loss_price,
                "Updated entry price"
            );
        }
    }

    /// Check order book updates, sell if take-profit or stop-loss reached
    pub async fn check_and_execute(&self, book: &BookUpdate) -> Result<()> {
        // Get best bid (last in bids array as bids are sorted by price descending)
        let best_bid = book.bids.last();
        let best_bid_price = match best_bid {
            Some(bid) => bid.price,
            None => return Ok(()), // No bids available, cannot sell
        };

        // Find all positions that need monitoring
        let positions_to_check: Vec<(String, HedgePosition)> = self
            .positions
            .iter()
            .filter(|entry| entry.value().token_id == book.asset_id)
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect();

        for (pair_id, position) in positions_to_check {
            // Check if GTC order already placed, if so use latest order book price to replace order
            if let Some(ref order_id) = position.order_id {
                let pending_amount = position.pending_sell_amount;
                if pending_amount > dec!(0) {
                    // Have unfilled orders, use latest order book price to replace order
                    info!(
                        "🔄 Detected unfilled order | Market:{} | Order ID:{} | Remaining:{} shares | Replacing with new price:{:.4}",
                        position.market_display,
                        &order_id[..16],
                        pending_amount,
                        best_bid_price
                    );
                    // Clear old order ID, prepare to replace order
                    if let Some(mut pos) = self.positions.get_mut(&pair_id) {
                        pos.order_id = None;
                    }
                    // Continue with order placement logic below, using pending_amount as sell quantity
                } else {
                    // Order submitted but pending_amount is 0, may be processing, skip
                    continue;
                }
            }

            // Check if take-profit or stop-loss reached
            let (should_sell, reason) = if best_bid_price >= position.take_profit_price {
                let profit_pct = ((best_bid_price - position.entry_price) / position.entry_price * dec!(100.0)).to_f64().unwrap_or(0.0);
                (true, format!("Take-profit({:.2}%)", profit_pct))
            } else if best_bid_price <= position.stop_loss_price {
                let loss_pct = ((position.entry_price - best_bid_price) / position.entry_price * dec!(100.0)).to_f64().unwrap_or(0.0);
                (true, format!("Stop-loss({:.2}%)", loss_pct))
            } else {
                (false, String::new())
            };

            if should_sell {
                // Get current token and opposite side token positions
                let current_position = self.position_tracker.get_position(position.token_id);
                let opposite_position = self.position_tracker.get_position(position.opposite_token_id);
                
                // Calculate difference: current position - opposite side position
                let difference = current_position - opposite_position;
                
                // If difference <= 0, opposite side can cover, no need to sell
                if difference <= dec!(0) {
                    info!(
                        "⏸️ No need to sell | Market:{} | Current position:{} shares | Opposite position:{} shares | Difference:{} shares | Opposite can cover",
                        position.market_display,
                        current_position,
                        opposite_position,
                        difference
                    );
                    continue;
                }
                
                // Determine actual amount to sell
                let sell_amount = if position.order_id.is_some() && position.pending_sell_amount > dec!(0) {
                    // If there are unfilled orders, use pending_sell_amount
                    position.pending_sell_amount
                } else {
                    // Otherwise use difference
                    difference
                };
                
                // Difference > 0, only sell the difference
                info!(
                    "✅ Reached {} | Market:{} | Current best bid:{:.4} | Entry price:{:.4} | Current position:{} shares | Opposite position:{} shares | Difference:{} shares | Preparing to sell:{} shares",
                    reason,
                    position.market_display,
                    best_bid_price,
                    position.entry_price,
                    current_position,
                    opposite_position,
                    difference,
                    sell_amount
                );
                
                // Use GTC order to sell
                // To avoid blocking main loop, spawn sell operation in separate async task
                let position_clone = position.clone();
                let pair_id_clone = pair_id.clone();
                let position_tracker = self.position_tracker.clone();
                let positions = self.positions.clone();
                let client = self.client.clone();
                let private_key = self.private_key.clone();
                
                // Mark as processing first to avoid duplicate orders (use remove+insert to avoid blocking)
                if let Some((_, mut pos)) = self.positions.remove(&pair_id) {
                    pos.order_id = Some("processing".to_string());
                    self.positions.insert(pair_id.clone(), pos);
                }
                
                tokio::spawn(async move {
                    // Recreate signer (cannot use self directly in spawn)
                    let signer = match LocalSigner::from_str(&private_key) {
                        Ok(s) => s.with_chain_id(Some(POLYGON)),
                        Err(e) => {
                            error!(
                                "❌ Failed to create signer | Market:{} | Error:{}",
                                position_clone.market_display,
                                e
                            );
                            return;
                        }
                    };
                    
                    // Execute sell operation
                    match Self::execute_sell_order(
                        &client,
                        &signer,
                        &position_clone,
                        best_bid_price,
                        sell_amount,
                    ).await {
                        Ok((order_id, filled, remaining)) => {
                            // Update position, mark order placed (use remove+insert to avoid get_mut blocking)
                            let order_id_short = order_id[..16].to_string();
                            if let Some((_, mut pos)) = positions.remove(&pair_id_clone) {
                                if remaining > dec!(0) {
                                    // Still has remaining, save order ID
                                    pos.order_id = Some(order_id);
                                    pos.pending_sell_amount = remaining;
                                    info!("🔒 Position order_id updated | Market:{} | Order ID:{} | Remaining:{} shares", 
                                        position_clone.market_display, order_id_short, remaining);
                                } else {
                                    // Fully filled, clear order ID
                                    pos.order_id = None;
                                    pos.pending_sell_amount = dec!(0);
                                    info!("✅ Sell order fully filled | Market:{} | Order ID:{} | Filled:{} shares", 
                                        position_clone.market_display, order_id_short, filled);
                                }
                                positions.insert(pair_id_clone.clone(), pos);
                            } else {
                                warn!("⚠️ Position not found | pair_id:{}", pair_id_clone);
                            }
                            
                            // Only update position and risk exposure for actually filled amount
                            if filled > dec!(0) {
                                info!("📊 Starting position update | Market:{} | Reducing:{} shares", 
                                    position_clone.market_display, filled);
                                position_tracker.update_position(position_clone.token_id, -filled);
                                info!("📊 Position update completed | Market:{}", position_clone.market_display);
                                
                                // Update risk exposure cost
                                info!("💰 Starting risk exposure update | Market:{} | entry_price:{} | sell_amount:{}", 
                                    position_clone.market_display,
                                    position_clone.entry_price,
                                    filled);
                                position_tracker.update_exposure_cost(
                                    position_clone.token_id,
                                    position_clone.entry_price,
                                    -filled,
                                );
                                info!("💰 Risk exposure update completed | Market:{}", position_clone.market_display);
                                
                                // Calculate risk exposure
                                let current_exposure = position_tracker.calculate_exposure();
                                info!(
                                    "📉 Risk exposure updated | Market:{} | Sold:{} shares | Current exposure:{:.2} USD",
                                    position_clone.market_display,
                                    filled,
                                    current_exposure
                                );
                            }
                        }
                        Err(e) => {
                            error!(
                                "❌ Sell order failed | Market:{} | Price:{:.4} | Error:{}",
                                position_clone.market_display,
                                best_bid_price,
                                e
                            );
                            // If failed, clear processing mark
                            if let Some(mut pos) = positions.get_mut(&pair_id_clone) {
                                pos.order_id = None;
                            }
                        }
                    }
                });
            }
        }

        Ok(())
    }

    /// Calculate actual sell amount (considering fees)
    fn calculate_sell_amount(&self, position: &HedgePosition) -> Decimal {
        self.calculate_sell_amount_with_size(position, position.amount)
    }

    /// Calculate actual sell amount for specified quantity (considering fees)
    fn calculate_sell_amount_with_size(&self, position: &HedgePosition, base_amount: Decimal) -> Decimal {
        // Calculate fee
        let p = position.entry_price.to_f64().unwrap_or(0.0);
        let c = 100.0;
        let fee_rate = 0.25;
        let exponent = 2.0;
        
        let base = p * (1.0 - p);
        let fee_value = c * fee_rate * base.powf(exponent);
        let fee_decimal = Decimal::try_from(fee_value).unwrap_or(dec!(0));
        
        // Calculate actual available amount
        let available_amount = if fee_decimal >= dec!(100.0) {
            dec!(0.01)
        } else {
            let multiplier = (dec!(100.0) - fee_decimal) / dec!(100.0);
            base_amount * multiplier
        };
        
        // Round down to 2 decimal places
        let floored_size = (available_amount * dec!(100.0)).floor() / dec!(100.0);
        
        if floored_size.is_zero() {
            dec!(0.01)
        } else {
            floored_size
        }
    }

    /// Static method: Calculate actual sell amount for specified quantity (considering fees)
    fn calculate_sell_amount_static(position: &HedgePosition, base_amount: Decimal) -> Decimal {
        // Calculate fee
        let p = position.entry_price.to_f64().unwrap_or(0.0);
        let c = 100.0;
        let fee_rate = 0.25;
        let exponent = 2.0;
        
        let base = p * (1.0 - p);
        let fee_value = c * fee_rate * base.powf(exponent);
        let fee_decimal = Decimal::try_from(fee_value).unwrap_or(dec!(0));
        
        // Calculate actual available amount
        let available_amount = if fee_decimal >= dec!(100.0) {
            dec!(0.01)
        } else {
            let multiplier = (dec!(100.0) - fee_decimal) / dec!(100.0);
            base_amount * multiplier
        };
        
        // Round down to 2 decimal places
        let floored_size = (available_amount * dec!(100.0)).floor() / dec!(100.0);
        
        if floored_size.is_zero() {
            dec!(0.01)
        } else {
            floored_size
        }
    }

    /// Static method: Execute sell order
    async fn execute_sell_order(
        client: &Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>,
        signer: &impl Signer<alloy::primitives::Signature>,
        position: &HedgePosition,
        price: Decimal,
        size: Decimal,
    ) -> Result<(String, Decimal, Decimal)> {
        // Calculate fee
        let p = position.entry_price.to_f64().unwrap_or(0.0);
        let c = 100.0;
        let fee_rate = 0.25;
        let exponent = 2.0;
        
        let base = p * (1.0 - p);
        let fee_value = c * fee_rate * base.powf(exponent);
        let fee_decimal = Decimal::try_from(fee_value).unwrap_or(dec!(0));
        
        // Calculate actual available amount
        let available_amount = if fee_decimal >= dec!(100.0) {
            dec!(0.01)
        } else {
            let multiplier = (dec!(100.0) - fee_decimal) / dec!(100.0);
            size * multiplier
        };
        
        // Round down to 2 decimal places
        let floored_size = (available_amount * dec!(100.0)).floor() / dec!(100.0);
        let order_size = if floored_size.is_zero() {
            dec!(0.01)
        } else {
            floored_size
        };

        info!(
            "💰 Calculating sell amount | Market:{} | Base amount:{:.2} shares | Entry price:{:.4} | Fee:{:.2}% | Available amount:{:.2} shares | Order size:{:.2} shares",
            position.market_display,
            size,
            position.entry_price,
            fee_decimal,
            available_amount,
            order_size
        );

        // Build GTC sell order
        let sell_order = client
            .limit_order()
            .token_id(position.token_id)
            .side(Side::Sell)
            .price(price)
            .size(order_size)
            .order_type(OrderType::GTC)
            .build()
            .await?;

        // Sign order
        let signed_order = client.sign(signer, sell_order).await?;

        // Submit order
        let result = client.post_order(signed_order).await?;

        if !result.success {
            let error_msg = result.error_msg.as_deref().unwrap_or("Unknown error");
            return Err(anyhow::anyhow!("GTC sell order failed: {}", error_msg));
        }

        // Check if order is immediately filled
        let filled = result.taking_amount;
        let remaining = order_size - filled;
        
        if filled > dec!(0) {
            info!(
                "💰 Sell order partially filled | Market:{} | Order ID:{} | Filled:{} shares | Remaining:{} shares",
                position.market_display,
                &result.order_id[..16],
                filled,
                remaining
            );
        } else {
            info!(
                "📋 Sell order submitted (not immediately filled) | Market:{} | Order ID:{} | Size:{} shares | Price:{:.4}",
                position.market_display,
                &result.order_id[..16],
                order_size,
                price
            );
        }
        
        Ok((result.order_id, filled, remaining))
    }

    /// Use GTC order to sell
    /// size: optional, if provided use that amount, otherwise use position.amount
    async fn sell_with_gtc(
        &self,
        position: &HedgePosition,
        price: Decimal,
        size: Option<Decimal>,
    ) -> Result<(String, Decimal, Decimal)> {
        let signer = LocalSigner::from_str(&self.private_key)?
            .with_chain_id(Some(POLYGON));

        // Calculate fee
        // Formula: fee = c * fee_rate * (p * (1-p))^exponent
        // Where: p = entry_price (buy price per unit), c = 100 (fixed value), fee_rate = 0.25, exponent = 2
        // Fee calculated is a float between 0-1.56 (ratio value, not absolute value)
        let p = position.entry_price.to_f64().unwrap_or(0.0);
        let c = 100.0; // Fixed at 100
        let fee_rate = 0.25;
        let exponent = 2.0;
        
        // Calculate fee ratio value (between 0-1.56)
        let base = p * (1.0 - p);
        let fee_value = c * fee_rate * base.powf(exponent);
        
        // Convert fee to Decimal
        let fee_decimal = Decimal::try_from(fee_value).unwrap_or(dec!(0));
        
        // Use provided size, if not provided use position.amount
        let base_amount = size.unwrap_or(position.amount);
        
        // Calculate actual available amount = filled amount * (100 - Fee) / 100
        // If Fee >= 100, abnormal situation, use minimum tradable unit
        let available_amount = if fee_decimal >= dec!(100.0) {
            dec!(0.01) // Abnormal situation, use minimum unit
        } else {
            // Normal case: available amount = filled amount * (100 - Fee) / 100
            let multiplier = (dec!(100.0) - fee_decimal) / dec!(100.0);
            base_amount * multiplier
        };
        
        // Round order size down to 2 decimal places (Polymarket requirement)
        // Use floor instead of round to avoid order size exceeding actual held amount
        // Method: multiply by 100, floor, divide by 100
        let floored_size = (available_amount * dec!(100.0)).floor() / dec!(100.0);
        
        // If floored size is 0, use minimum tradable unit
        let order_size = if floored_size.is_zero() {
            dec!(0.01) // Minimum unit
        } else {
            floored_size
        };

        info!(
            "💰 Calculating sell amount | Market:{} | Base amount:{:.2} shares | Entry price:{:.4} | Fee:{:.2}% | Available amount:{:.2} shares | Order size:{:.2} shares",
            position.market_display,
            base_amount,
            position.entry_price,
            fee_decimal,
            available_amount,
            order_size
        );

        // Build GTC sell order
        let sell_order = self
            .client
            .limit_order()
            .token_id(position.token_id)
            .side(Side::Sell)
            .price(price)
            .size(order_size)
            .order_type(OrderType::GTC)
            .build()
            .await?;

        // Sign order
        let signed_order = self.client.sign(&signer, sell_order).await?;

        // Submit order
        let result = self.client.post_order(signed_order).await?;

        if !result.success {
            let error_msg = result.error_msg.as_deref().unwrap_or("Unknown error");
            return Err(anyhow::anyhow!("GTC sell order failed: {}", error_msg));
        }

        // Check if order is immediately filled
        let filled = result.taking_amount;
        let remaining = order_size - filled;
        
        if filled > dec!(0) {
            info!(
                "💰 Sell order partially filled | Market:{} | Order ID:{} | Filled:{} shares | Remaining:{} shares",
                position.market_display,
                &result.order_id[..16],
                filled,
                remaining
            );
        } else {
            info!(
                "📋 Sell order submitted (not immediately filled) | Market:{} | Order ID:{} | Size:{} shares | Price:{:.4}",
                position.market_display,
                &result.order_id[..16],
                order_size,
                price
            );
        }
        
        Ok((result.order_id, filled, remaining))
    }

    /// Remove completed position
    pub fn remove_position(&self, pair_id: &str) {
        self.positions.remove(pair_id);
        info!(pair_id = %pair_id, "Removed hedge position");
    }

    /// Get all monitored positions
    pub fn get_positions(&self) -> Vec<HedgePosition> {
        self.positions.iter().map(|e| e.value().clone()).collect()
    }
}
