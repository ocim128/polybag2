//! ActionExecutor — translates `Action` values into real API calls.
//!
//! When `dry_run` is true every action is logged but never sent to
//! the exchange, making it safe to test strategies with real market
//! data and a $12 balance.

use std::sync::Arc;
use rust_decimal_macros::dec;
use tracing::{debug, error, info, warn};

use crate::monitor::arbitrage::ArbitrageOpportunity;
use crate::risk::RiskManager;
use crate::risk::positions::PositionTracker;
use crate::trading::TradingExecutor;
use super::Action;

pub struct ActionExecutor {
    executor: Arc<TradingExecutor>,
    risk_manager: Arc<RiskManager>,
    position_tracker: Arc<PositionTracker>,
    dry_run: bool,
}

impl ActionExecutor {
    pub fn new(
        executor: Arc<TradingExecutor>,
        risk_manager: Arc<RiskManager>,
        position_tracker: Arc<PositionTracker>,
        dry_run: bool,
    ) -> Self {
        Self { executor, risk_manager, position_tracker, dry_run }
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
                    // Risk gate: check exposure limit before posting (mirrors arb path)
                    let cost = price * size;
                    let current = self.position_tracker.calculate_exposure();
                    let max_exp = self.position_tracker.max_exposure();
                    if current + cost > max_exp {
                        warn!(
                            "⛔ MM PostLimitOrder blocked by exposure limit | cost:{:.4} | current:{:.4} | max:{:.4}",
                            cost, current, max_exp
                        );
                        continue;
                    }

                    if self.dry_run {
                        info!(
                            "[DRY RUN] Would post limit BUY | {} | token:{} | price:{:.4} | size:{}",
                            market_display, token_id, price, size
                        );
                        continue;
                    }

                    match self.executor.buy_at_price(token_id, price, size).await {
                        Ok(resp) => {
                            // Update exposure ONLY on confirmed post
                            self.position_tracker.update_exposure_cost(token_id, price, size);
                            info!(
                                "🏪 Limit BUY posted | {} | price:{:.4} | size:{} | resp:{:?}",
                                market_display, price, size, resp
                            );
                        }
                        Err(e) => {
                            // No exposure update on failure — prevents permanent drift
                            warn!(error = %e, "Failed to post limit BUY | {} | no exposure charged", market_display);
                        }
                    }
                }
                Action::CancelAll => {
                    if self.dry_run {
                        info!("[DRY RUN] Would cancel all orders | {}", market_display);
                        continue;
                    }
                    if let Err(e) = self.executor.cancel_all_orders().await {
                        warn!(error = %e, "Failed to cancel all orders");
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
            return;
        }

        // Update exposure ONLY when we will actually execute
        self.position_tracker.update_exposure_cost(opp.yes_token_id, opp.yes_ask_price, order_size);
        self.position_tracker.update_exposure_cost(opp.no_token_id, opp.no_ask_price, order_size);

        // Spawn async task so we don't block the book monitor
        let executor = self.executor.clone();
        let risk_manager = self.risk_manager.clone();
        let opp_clone = opp.clone();

        tokio::spawn(async move {
            match executor.execute_arbitrage_pair(&opp_clone, &yes_dir, &no_dir).await {
                Ok(result) => {
                    let pair_id = result.pair_id.clone();
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
