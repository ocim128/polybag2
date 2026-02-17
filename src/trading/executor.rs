use anyhow::Result;
use alloy::signers::Signer;
use alloy::signers::local::LocalSigner;
use chrono::Utc;
use polymarket_client_sdk::clob::{Client, Config};
use polymarket_client_sdk::clob::types::{OrderType, Side, SignatureType};
use polymarket_client_sdk::types::{Address, Decimal, U256};
use polymarket_client_sdk::POLYGON;
use rust_decimal_macros::dec;
use std::str::FromStr;
use std::time::Instant;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::monitor::arbitrage::ArbitrageOpportunity;

pub struct OrderPairResult {
    pub pair_id: String,
    pub yes_order_id: String,
    pub no_order_id: String,
    pub yes_filled: Decimal,
    pub no_filled: Decimal,
    pub yes_size: Decimal,
    pub no_size: Decimal,
    pub success: bool,
}

pub struct TradingExecutor {
    client: Client<polymarket_client_sdk::auth::state::Authenticated<polymarket_client_sdk::auth::Normal>>,
    private_key: String,
    max_order_size: Decimal,
    slippage: [Decimal; 2], // [first, second], down(↓) uses second, up(↑) and flat(−/empty) use first
    gtd_expiration_secs: u64,
    arbitrage_order_type: OrderType,
}

impl TradingExecutor {
    pub async fn new(
        private_key: String,
        max_order_size_usdc: f64,
        proxy_address: Option<Address>,
        slippage: [f64; 2],
        gtd_expiration_secs: u64,
        arbitrage_order_type: OrderType,
    ) -> Result<Self> {
        // Verify private key format
        let signer = LocalSigner::from_str(&private_key)
            .map_err(|e| anyhow::anyhow!("Invalid private key format: {}. Please ensure the private key is a 64-character hex string (without 0x prefix)", e))?
            .with_chain_id(Some(POLYGON));

        let config = Config::builder().use_server_time(false).build();
        let mut auth_builder = Client::new("https://clob.polymarket.com", config)
            .map_err(|e| anyhow::anyhow!("Creating CLOB client failed: {}", e))?
            .authentication_builder(&signer);
        
        // If proxy_address is provided, set funder and signature_type (following Python SDK pattern)
        if let Some(funder) = proxy_address {
            auth_builder = auth_builder
                .funder(funder)
                .signature_type(SignatureType::Proxy);
        }
        
        let client = auth_builder
            .authenticate()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "API authentication failed: {}. Possible causes: 1) Invalid private key 2) Network issue 3) Polymarket API service unavailable",
                    e
                )
            })?;

        Ok(Self {
            client,
            private_key,
            max_order_size: Decimal::try_from(max_order_size_usdc)
                .unwrap_or(rust_decimal_macros::dec!(100.0)),
            slippage: [
                Decimal::try_from(slippage[0]).unwrap_or(dec!(0.0)),
                Decimal::try_from(slippage[1]).unwrap_or(dec!(0.01)),
            ],
            gtd_expiration_secs,
            arbitrage_order_type,
        })
    }

    /// Verify authentication really succeeded - use api_keys() to verify as per official example
    pub async fn verify_authentication(&self) -> Result<()> {
        // Use api_keys() to verify authentication status as per official example
        self.client.api_keys().await
            .map_err(|e| anyhow::anyhow!("Authentication verification failed: API call returned error: {}", e))?;
        Ok(())
    }

    /// Cancel all open orders for this account (used during wind down)
    pub async fn cancel_all_orders(&self) -> Result<polymarket_client_sdk::clob::types::response::CancelOrdersResponse> {
        self.client
            .cancel_all_orders()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to cancel all open orders: {}", e))
    }

    /// Place a GTC sell order at the specified price (used to sell leg position during wind down)
    pub async fn sell_at_price(
        &self,
        token_id: U256,
        price: Decimal,
        size: Decimal,
    ) -> Result<polymarket_client_sdk::clob::types::response::PostOrderResponse> {
        let signer = LocalSigner::from_str(&self.private_key)?
            .with_chain_id(Some(POLYGON));
        let order = self
            .client
            .limit_order()
            .token_id(token_id)
            .side(Side::Sell)
            .price(price)
            .size(size)
            .order_type(OrderType::GTC)
            .build()
            .await?;
        let signed = self.client.sign(&signer, order).await?;
        self.client
            .post_order(signed)
            .await
            .map_err(|e| anyhow::anyhow!("GTC sell order submission failed: {}", e))
    }

    /// Get slippage by direction: down(↓) uses second, up(↑) and flat(−/empty) use first
    fn slippage_for_direction(&self, dir: &str) -> Decimal {
        if dir == "↓" {
            self.slippage[1]
        } else {
            self.slippage[0]
        }
    }

    /// Execute arbitrage with post_orders batch submission for YES and NO orders.
    /// Order type is configured via arbitrage_order_type; GTD uses gtd_expiration_secs.
    /// yes_dir / no_dir: price direction "↑" "↓" "−" or "", used for direction-based slippage (down=second, up/flat=first).
    pub async fn execute_arbitrage_pair(
        &self,
        opp: &ArbitrageOpportunity,
        yes_dir: &str,
        no_dir: &str,
    ) -> Result<OrderPairResult> {
        // Performance timing: total start time
        let total_start = Instant::now();
        
        // This log is already printed in main.rs, don't repeat here
        let expiry_info = if matches!(self.arbitrage_order_type, OrderType::GTD) {
            format!("Expiry:{}s", self.gtd_expiration_secs)
        } else {
            "No expiry".to_string()
        };
        debug!(
            market_id = %opp.market_id,
            profit_pct = %opp.profit_percentage,
            order_type = %self.arbitrage_order_type,
            "Starting arbitrage execution (batch orders, order type:{}, {})",
            self.arbitrage_order_type,
            expiry_info
        );

        // Calculate actual order size (considering max order limit)
        let yes_token_id = U256::from_str(&opp.yes_token_id.to_string())?;
        let no_token_id = U256::from_str(&opp.no_token_id.to_string())?;

        let order_size = opp.yes_size.min(opp.no_size).min(self.max_order_size);

        // Generate order pair ID
        let pair_id = Uuid::new_v4().to_string();

        // Calculate expiration time: current time + configured expiration duration
        let expiration = Utc::now() + chrono::Duration::seconds(self.gtd_expiration_secs as i64);

        // Slippage by direction: up=first, down/flat=second
        let yes_slippage_apply = self.slippage_for_direction(yes_dir);
        let no_slippage_apply = self.slippage_for_direction(no_dir);
        let yes_price_with_slippage = (opp.yes_ask_price + yes_slippage_apply).min(dec!(1.0));
        let no_price_with_slippage = (opp.no_ask_price + no_slippage_apply).min(dec!(1.0));
        
        // Print price level info (prices with slippage applied)
        info!(
            "📋 Level | YES {:.4}×{:.2} NO {:.4}×{:.2}",
            yes_price_with_slippage, order_size,
            no_price_with_slippage, order_size
        );
        
        let expiry_suffix = if matches!(self.arbitrage_order_type, OrderType::GTD) {
            format!(" | GTD {}s", self.gtd_expiration_secs)
        } else {
            String::new()
        };
        info!(
            "📤 Order | YES {:.4}→{:.4}×{} NO {:.4}→{:.4}×{} | {}{}",
            opp.yes_ask_price, yes_price_with_slippage, order_size,
            opp.no_ask_price, no_price_with_slippage, order_size,
            self.arbitrage_order_type, expiry_suffix
        );

        // Pre-order check: both sides must be > $1 (exchange minimum order amount)
        let yes_amount_usd = yes_price_with_slippage * order_size;
        let no_amount_usd = no_price_with_slippage * order_size;
        if yes_amount_usd <= dec!(1) || no_amount_usd <= dec!(1) {
            warn!(
                "⏭️ Skip order | YES amount:{:.2} USD NO amount:{:.2} USD | Both sides must be > $1",
                yes_amount_usd, no_amount_usd
            );
            return Err(anyhow::anyhow!(
                "Order amount does not meet exchange minimum: YES {:.2} USD, NO {:.2} USD, both sides must be > $1",
                yes_amount_usd, no_amount_usd
            ));
        }

        // Performance timing: start building YES and NO orders in parallel
        let build_start = Instant::now();
        
        // Build YES and NO orders in parallel; only set expiration for GTD (SDK: non-GTD cannot have expiration)
        let (yes_order, no_order) = tokio::join!(
            async {
                let b = self.client
                    .limit_order()
                    .token_id(yes_token_id)
                    .side(Side::Buy)
                    .price(yes_price_with_slippage)
                    .size(order_size)
                    .order_type(self.arbitrage_order_type.clone());
                if matches!(&self.arbitrage_order_type, OrderType::GTD) {
                    b.expiration(expiration).build().await
                } else {
                    b.build().await
                }
            },
            async {
                let b = self.client
                    .limit_order()
                    .token_id(no_token_id)
                    .side(Side::Buy)
                    .price(no_price_with_slippage)
                    .size(order_size)
                    .order_type(self.arbitrage_order_type.clone());
                if matches!(&self.arbitrage_order_type, OrderType::GTD) {
                    b.expiration(expiration).build().await
                } else {
                    b.build().await
                }
            }
        );
        
        let yes_order = yes_order?;
        let no_order = no_order?;
        let build_elapsed = build_start.elapsed().as_millis();

        // Performance timing: start parallel signing
        let sign_start = Instant::now();
        
        // Create signer
        let signer = LocalSigner::from_str(&self.private_key)?
            .with_chain_id(Some(POLYGON));
        
        // Sign YES and NO orders in parallel
        let (signed_yes_result, signed_no_result) = tokio::join!(
            self.client.sign(&signer, yes_order),
            self.client.sign(&signer, no_order)
        );
        
        let signed_yes = signed_yes_result?;
        let signed_no = signed_no_result?;
        let sign_elapsed = sign_start.elapsed().as_millis();

        // Performance timing: start sending orders
        let send_start = Instant::now();
        
        // Higher price first; parse yes_result/no_result from results in same order
        let yes_first = yes_price_with_slippage >= no_price_with_slippage;
        let orders_to_send: Vec<_> = if yes_first {
            vec![signed_yes, signed_no]
        } else {
            vec![signed_no, signed_yes]
        };
        let results = match self.client.post_orders(orders_to_send).await {
            Ok(results) => {
                let send_elapsed = send_start.elapsed().as_millis();
                let total_elapsed = total_start.elapsed().as_millis();
                
                info!(
                    "⏱️ Time | {} | Build{}ms Sign{}ms Send{}ms Total{}ms",
                    &pair_id[..8], build_elapsed, sign_elapsed, send_elapsed, total_elapsed
                );
                
                results
            }
            Err(e) => {
                let send_elapsed = send_start.elapsed().as_millis();
                let total_elapsed = total_start.elapsed().as_millis();
                
                error!(
                    "❌ Batch order API call failed | PairID:{} | YES price:{} (with slippage) | NO price:{} (with slippage) | Size:{} | Build:{}ms | Sign:{}ms | Send:{}ms | Total:{}ms | Error:{}",
                    &pair_id[..8],
                    yes_price_with_slippage,
                    no_price_with_slippage,
                    order_size,
                    build_elapsed,
                    sign_elapsed,
                    send_elapsed,
                    total_elapsed,
                    e
                );
                return Err(anyhow::anyhow!("Batch order API call failed: {}", e));
            }
        };
        
        // Validate result count
        if results.len() != 2 {
            error!(
                "❌ Batch order returned incorrect number of results | PairID:{} | Expected:2 | Actual:{}",
                &pair_id[..8],
                results.len()
            );
            return Err(anyhow::anyhow!(
                "Batch order returned incorrect number of results | Expected:2 | Actual:{}",
                results.len()
            ));
        }
        
        // Extract YES and NO order results (submitted with higher price first, map using yes_first)
        let (yes_result, no_result) = if yes_first {
            (&results[0], &results[1])
        } else {
            (&results[1], &results[0])
        };

        // Order result details removed, only key info retained in subsequent logs

        // Check filled amounts (key metric for GTD orders)
        let yes_filled = yes_result.taking_amount;
        let no_filled = no_result.taking_amount;

        // For GTD orders, if cannot fill completely within expiration time, orders cancel after expiry
        // We should check actual filled amounts instead of success field
        // Only return error if neither order filled at all
        if yes_filled == dec!(0) && no_filled == dec!(0) {
            // Extract simplified error messages
            let yes_error_msg = yes_result
                .error_msg
                .as_deref()
                .unwrap_or("Unknown error");
            let no_error_msg = no_result
                .error_msg
                .as_deref()
                .unwrap_or("Unknown error");
            
            // Simplify error messages, remove technical details
            let yes_error_simple = if yes_error_msg.contains("no orders found to match") {
                "No matching orders in book"
            } else if yes_error_msg.contains("GTD") || yes_error_msg.contains("FOK") || yes_error_msg.contains("FAK") || yes_error_msg.contains("GTC") {
                "Order cannot be filled"
            } else {
                yes_error_msg
            };
            
            let no_error_simple = if no_error_msg.contains("no orders found to match") {
                "No matching orders in book"
            } else if no_error_msg.contains("GTD") || no_error_msg.contains("FOK") || no_error_msg.contains("FAK") || no_error_msg.contains("GTC") {
                "Order cannot be filled"
            } else {
                no_error_msg
            };

            error!(
                "❌ Arbitrage failed | PairID:{} | YES order:{} | NO order:{}",
                &pair_id[..8], // Show only first 8 characters
                yes_error_simple,
                no_error_simple
            );

            // Detailed error info logged at debug level
            debug!(
                pair_id = %pair_id,
                yes_order_id = ?yes_result.order_id,
                no_order_id = ?no_result.order_id,
                yes_success = yes_result.success,
                no_success = no_result.success,
                yes_error = %yes_error_msg,
                no_error = %no_error_msg,
                "Neither order filled (details)"
            );

            return Err(anyhow::anyhow!(
                "Arbitrage failed: Neither YES nor NO order filled | YES: {}, NO: {}",
                yes_error_simple,
                no_error_simple
            ));
        }

        // If at least one order filled, log warning but don't return error
        // Let subsequent risk manager handle one-sided fill situation
        if !yes_result.success || !no_result.success {
            let yes_error_msg = yes_result
                .error_msg
                .as_deref()
                .unwrap_or("Unknown error");
            let no_error_msg = no_result
                .error_msg
                .as_deref()
                .unwrap_or("Unknown error");

            // Simplify error messages
            let yes_error_simple = if yes_error_msg.contains("no orders found to match") {
                "Partially unfilled (order placed)"
            } else if yes_error_msg.contains("GTD") || yes_error_msg.contains("FOK") || yes_error_msg.contains("FAK") || yes_error_msg.contains("GTC") {
                "Partially unfilled (order placed)"
            } else {
                "Status abnormal"
            };
            
            let no_error_simple = if no_error_msg.contains("no orders found to match") {
                "Partially unfilled (order placed)"
            } else if no_error_msg.contains("GTD") || no_error_msg.contains("FOK") || no_error_msg.contains("FAK") || no_error_msg.contains("GTC") {
                "Partially unfilled (order placed)"
            } else {
                "Status abnormal"
            };

            warn!(
                "⚠️ Partial order status abnormal | PairID:{} | YES:{} (filled:{} shares) | NO:{} (filled:{} shares) | Risk management activated",
                &pair_id[..8],
                yes_error_simple,
                yes_filled,
                no_error_simple,
                no_filled
            );

            // Detailed error info logged at debug level
            debug!(
                pair_id = %pair_id,
                yes_order_id = ?yes_result.order_id,
                no_order_id = ?no_result.order_id,
                yes_success = yes_result.success,
                no_success = no_result.success,
                yes_error = %yes_error_msg,
                no_error = %no_error_msg,
                "Order submission status abnormal details"
            );
        }

        // Print different logs based on fill status
        if yes_filled > dec!(0) && no_filled > dec!(0) {
            info!(
                "✅ Arbitrage successful | PairID:{} | YES filled:{} shares | NO filled:{} shares | Total filled:{} shares",
                &pair_id[..8],
                yes_filled,
                no_filled,
                yes_filled.min(no_filled)
            );
        } else if yes_filled > dec!(0) || no_filled > dec!(0) {
            let side = if yes_filled > dec!(0) { "YES" } else { "NO" };
            let filled = if yes_filled > dec!(0) { yes_filled } else { no_filled };
            let other_side = if yes_filled > dec!(0) { "NO" } else { "YES" };
            warn!(
                "⚠️ One-sided fill | {} | {} filled {} shares, {} not filled (risk control)",
                &pair_id[..8], side, filled, other_side
            );
        } else {
            warn!(
                "❌ Arbitrage failed | PairID:{} | Neither YES nor NO filled",
                &pair_id[..8]
            );
        }

        Ok(OrderPairResult {
            pair_id,
            yes_order_id: yes_result.order_id.clone(),
            no_order_id: no_result.order_id.clone(),
            yes_filled,
            no_filled,
            yes_size: order_size,
            no_size: order_size,
            success: true,
        })
    }
}
