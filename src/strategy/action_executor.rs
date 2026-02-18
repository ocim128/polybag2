//! ActionExecutor — translates `Action` values into real API calls.
//!
//! When `dry_run` is true every action is logged but never sent to
//! the exchange, making it safe to test strategies with real market
//! data and a $12 balance.

use std::sync::Arc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::monitor::arbitrage::ArbitrageOpportunity;
use crate::risk::RiskManager;
use crate::risk::positions::PositionTracker;
use crate::trading::TradingExecutor;
use crate::utils::trade_journal::TradeJournal;
use super::Action;

pub struct ActionExecutor {
    executor: Arc<TradingExecutor>,
    risk_manager: Arc<RiskManager>,
    position_tracker: Arc<PositionTracker>,
    dry_run: bool,
    trade_journal: TradeJournal,
}

impl ActionExecutor {
    pub fn new(
        executor: Arc<TradingExecutor>,
        risk_manager: Arc<RiskManager>,
        position_tracker: Arc<PositionTracker>,
        dry_run: bool,
        slot_name: &str,
    ) -> Self {
        Self {
            executor,
            risk_manager,
            position_tracker,
            dry_run,
            trade_journal: TradeJournal::new(slot_name),
        }
    }

    /// Polymarket CLOB tick size is 0.01. Normalize price down to valid tick.
    fn normalize_tick_price(price: Decimal) -> Decimal {
        let tick = dec!(0.01);
        (((price / tick).floor()) * tick).max(tick).min(dec!(1.0))
    }

    /// Execute a batch of actions returned by the active strategy.
    pub async fn execute(&self, actions: Vec<Action>, market_display: &str) {
        for action in actions {
            match action {
                Action::Skip { reason } => {
                    debug!("⏸️ {} | {}", market_display, reason);
                }
                Action::PlaceArbPair { opp, yes_dir, no_dir } => {
                    self.execute_arb_pair(opp, yes_dir, no_dir, market_display).await;
                }
                Action::PostLimitOrder { token_id, price, size } => {
                    let normalized_price = Self::normalize_tick_price(price);
                    if normalized_price != price {
                        warn!(
                            "Price normalized to tick size | {} | raw:{:.8} -> tick:{:.2}",
                            market_display, price, normalized_price
                        );
                    }

                    // Risk gate: check exposure limit before posting (mirrors arb path)
                    let cost = normalized_price * size;
                    let current = self.position_tracker.calculate_exposure();
                    let max_exp = self.position_tracker.max_exposure();
                    if current + cost > max_exp {
                        warn!(
                            "⛔ MM PostLimitOrder blocked by exposure limit | cost:{:.4} | current:{:.4} | max:{:.4}",
                            cost, current, max_exp
                        );
                        self.trade_journal.log_event(
                            "order_blocked_exposure",
                            market_display,
                            json!({
                                "type": "post_limit_buy",
                                "token_id": token_id.to_string(),
                                "raw_price": price.to_string(),
                                "price": normalized_price.to_string(),
                                "size": size.to_string(),
                                "cost": cost.to_string(),
                                "current_exposure": current.to_string(),
                                "max_exposure": max_exp.to_string(),
                            }),
                        );
                        continue;
                    }

                    if self.dry_run {
                        info!(
                            "[DRY RUN] Would post limit BUY | {} | token:{} | price:{:.4} | size:{}",
                            market_display, token_id, normalized_price, size
                        );
                        self.trade_journal.log_event(
                            "dry_run_post_limit_buy",
                            market_display,
                            json!({
                                "token_id": token_id.to_string(),
                                "raw_price": price.to_string(),
                                "price": normalized_price.to_string(),
                                "size": size.to_string(),
                                "cost": (normalized_price * size).to_string(),
                            }),
                        );
                        continue;
                    }

                    match self.executor.buy_at_price(token_id, normalized_price, size).await {
                        Ok(resp) => {
                            // Update exposure ONLY on confirmed post
                            self.position_tracker.update_exposure_cost(token_id, normalized_price, size);
                            info!(
                                "🏪 Limit BUY posted | {} | price:{:.4} | size:{} | resp:{:?}",
                                market_display, normalized_price, size, resp
                            );
                            self.trade_journal.log_event(
                                "limit_buy_posted",
                                market_display,
                                json!({
                                    "token_id": token_id.to_string(),
                                    "raw_price": price.to_string(),
                                    "price": normalized_price.to_string(),
                                    "size": size.to_string(),
                                    "response": format!("{:?}", resp),
                                }),
                            );
                        }
                        Err(e) => {
                            // No exposure update on failure — prevents permanent drift
                            warn!(error = %e, "Failed to post limit BUY | {} | no exposure charged", market_display);
                            let emsg = e.to_string();
                            if emsg.to_lowercase().contains("invalid signature") {
                                warn!(
                                    "Order signature rejected. Likely cause: CLOB_SIGNATURE_TYPE mismatch. \
                                     Your wallet may require a different type. Try setting CLOB_SIGNATURE_TYPE \
                                     to one of: eoa, proxy, gnosis_safe (current is 'auto'). Also verify \
                                     WALLET_N_KEY matches WALLET_N_PROXY, and host clock is in sync."
                                );
                            }
                            self.trade_journal.log_event(
                                "limit_buy_failed",
                                market_display,
                                json!({
                                    "token_id": token_id.to_string(),
                                    "raw_price": price.to_string(),
                                    "price": normalized_price.to_string(),
                                    "size": size.to_string(),
                                    "error": emsg,
                                }),
                            );
                        }
                    }
                }
                Action::CancelAll => {
                    if self.dry_run {
                        info!("[DRY RUN] Would cancel all orders | {}", market_display);
                        self.trade_journal.log_event(
                            "dry_run_cancel_all",
                            market_display,
                            json!({}),
                        );
                        continue;
                    }
                    match self.executor.cancel_all_orders().await {
                        Ok(resp) => {
                            self.trade_journal.log_event(
                                "cancel_all_ok",
                                market_display,
                                json!({ "response": format!("{:?}", resp) }),
                            );
                        }
                        Err(e) => {
                            warn!(error = %e, "Failed to cancel all orders");
                            self.trade_journal.log_event(
                                "cancel_all_failed",
                                market_display,
                                json!({ "error": e.to_string() }),
                            );
                        }
                    }
                }
            }
        }
    }

    async fn execute_arb_pair(
        &self,
        opp: ArbitrageOpportunity,
        yes_dir: String,
        no_dir: String,
        market_display: &str,
    ) {
        let order_size = opp.yes_size.min(opp.no_size);

        // DRY_RUN: log without mutating exposure state
        if self.dry_run {
            info!(
                "[DRY RUN] Would execute arb | {} | YES:{:.4} NO:{:.4} | profit:{:.2}% | size:{} | cost:{:.2} USD",
                market_display, opp.yes_ask_price, opp.no_ask_price,
                opp.profit_percentage, order_size,
                opp.yes_ask_price * order_size + opp.no_ask_price * order_size
            );
            self.trade_journal.log_event(
                "dry_run_arb_pair",
                market_display,
                json!({
                    "yes_token_id": opp.yes_token_id.to_string(),
                    "no_token_id": opp.no_token_id.to_string(),
                    "yes_price": opp.yes_ask_price.to_string(),
                    "no_price": opp.no_ask_price.to_string(),
                    "size": order_size.to_string(),
                    "profit_pct": opp.profit_percentage.to_string(),
                    "cost_usd": (opp.yes_ask_price * order_size + opp.no_ask_price * order_size).to_string(),
                }),
            );
            return;
        }

        // Update exposure ONLY when we will actually execute
        self.position_tracker.update_exposure_cost(opp.yes_token_id, opp.yes_ask_price, order_size);
        self.position_tracker.update_exposure_cost(opp.no_token_id, opp.no_ask_price, order_size);
        self.trade_journal.log_event(
            "arb_pair_submitted",
            market_display,
            json!({
                "market_id": format!("{:?}", opp.market_id),
                "yes_token_id": opp.yes_token_id.to_string(),
                "no_token_id": opp.no_token_id.to_string(),
                "yes_price": opp.yes_ask_price.to_string(),
                "no_price": opp.no_ask_price.to_string(),
                "size": order_size.to_string(),
                "profit_pct": opp.profit_percentage.to_string(),
                "yes_dir": yes_dir.clone(),
                "no_dir": no_dir.clone(),
            }),
        );

        // Spawn async task so we don't block the book monitor
        let executor = self.executor.clone();
        let risk_manager = self.risk_manager.clone();
        let opp_clone = opp.clone();
        let market_display_owned = market_display.to_string();
        let trade_journal = self.trade_journal.clone();

        tokio::spawn(async move {
            match executor.execute_arbitrage_pair(&opp_clone, &yes_dir, &no_dir).await {
                Ok(result) => {
                    let pair_id = result.pair_id.clone();
                    trade_journal.log_event(
                        "arb_pair_executed",
                        &market_display_owned,
                        json!({
                            "pair_id": pair_id.clone(),
                            "yes_order_id": result.yes_order_id,
                            "no_order_id": result.no_order_id,
                            "yes_filled": result.yes_filled.to_string(),
                            "no_filled": result.no_filled.to_string(),
                            "yes_size": result.yes_size.to_string(),
                            "no_size": result.no_size.to_string(),
                            "success": result.success,
                        }),
                    );
                    risk_manager.register_order_pair(
                        result,
                        opp_clone.market_id,
                        opp_clone.yes_token_id,
                        opp_clone.no_token_id,
                        opp_clone.yes_ask_price,
                        opp_clone.no_ask_price,
                    );
                    match risk_manager.handle_order_pair(&pair_id).await {
                        Ok(action) => match action {
                            crate::risk::recovery::RecoveryAction::None => {}
                            crate::risk::recovery::RecoveryAction::MonitorForExit { .. } => {
                                info!("Single-sided fill, hedging disabled, no action");
                            }
                            crate::risk::recovery::RecoveryAction::SellExcess { .. } => {
                                info!("Partial fill imbalance, hedging disabled, no action");
                            }
                            crate::risk::recovery::RecoveryAction::ManualIntervention { reason } => {
                                warn!("Manual intervention required: {}", reason);
                            }
                        },
                        Err(e) => error!("Risk handling failed: {}", e),
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    trade_journal.log_event(
                        "arb_pair_failed",
                        &market_display_owned,
                        json!({
                            "error": msg.clone(),
                            "yes_token_id": opp_clone.yes_token_id.to_string(),
                            "no_token_id": opp_clone.no_token_id.to_string(),
                        }),
                    );
                    if msg.contains("arbitrage failed") {
                        error!("{}", msg);
                    } else {
                        error!("Failed to execute arbitrage trade: {}", msg);
                    }
                }
            }
        });
    }
}
