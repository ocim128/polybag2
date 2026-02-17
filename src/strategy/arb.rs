//! Arbitrage strategy – extracted from the original main.rs.
//!
//! Monitors order books for YES+NO spread opportunities where
//! `yes_ask + no_ask < 1.0` and returns `Action::PlaceArbPair`
//! when all safety checks pass.

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::{Duration, Instant};
use tracing::info;

use crate::monitor::arbitrage::ArbitrageDetector;
use crate::monitor::orderbook::OrderBookPair;
use super::{Action, StrategyContext};

/// Minimum interval between two arbitrage trades.
const MIN_TRADE_INTERVAL: Duration = Duration::from_secs(3);

pub struct ArbitrageStrategy {
    detector: ArbitrageDetector,
    last_trade_time: Option<Instant>,
}

impl ArbitrageStrategy {
    pub fn new(min_profit_threshold: f64) -> Self {
        Self {
            detector: ArbitrageDetector::new(min_profit_threshold),
            last_trade_time: None,
        }
    }

    pub fn name(&self) -> &str { "arbitrage" }

    pub fn on_window_start(&mut self) {
        self.last_trade_time = None;
    }

    pub async fn on_book_update(
        &mut self,
        pair: &OrderBookPair,
        ctx: &StrategyContext<'_>,
    ) -> Vec<Action> {
        let config = ctx.config;
        let market_display = ctx.market_display;

        // ── execution threshold check ──────────────────────────
        let execution_threshold = dec!(1.0)
            - Decimal::try_from(config.arbitrage_execution_spread).unwrap_or(dec!(0.01));

        let yes_ask = pair.yes_book.asks.last().map(|a| a.price);
        let no_ask = pair.no_book.asks.last().map(|a| a.price);
        match (yes_ask, no_ask) {
            (Some(y), Some(n)) if y + n <= execution_threshold => {}
            _ => return vec![],
        };

        // ── detect arbitrage opportunity ───────────────────────
        let opp = match self.detector.check_arbitrage(
            &pair.yes_book,
            &pair.no_book,
            &pair.market_id,
        ) {
            Some(opp) => opp,
            None => return vec![],
        };

        // ── YES price threshold ────────────────────────────────
        if config.min_yes_price_threshold > 0.0 {
            let min_yes = Decimal::try_from(config.min_yes_price_threshold).unwrap_or(dec!(0.0));
            if opp.yes_ask_price < min_yes {
                return vec![Action::Skip {
                    reason: format!("YES price {:.4} < threshold {:.4}", opp.yes_ask_price, min_yes),
                }];
            }
        }

        // ── NO price threshold ─────────────────────────────────
        if config.min_no_price_threshold > 0.0 {
            let min_no = Decimal::try_from(config.min_no_price_threshold).unwrap_or(dec!(0.0));
            if opp.no_ask_price < min_no {
                return vec![Action::Skip {
                    reason: format!("NO price {:.4} < threshold {:.4}", opp.no_ask_price, min_no),
                }];
            }
        }

        // ── stop before market end ─────────────────────────────
        if config.stop_arbitrage_before_end_minutes > 0 {
            if let Some(mi) = ctx.market_info {
                let secs_left = mi.end_date.signed_duration_since(Utc::now()).num_seconds();
                let threshold = config.stop_arbitrage_before_end_minutes as i64 * 60;
                if secs_left <= threshold {
                    return vec![Action::Skip {
                        reason: format!("{}s until market end (stop {}min)", secs_left, config.stop_arbitrage_before_end_minutes),
                    }];
                }
            }
        }

        // ── risk exposure check ────────────────────────────────
        let max_order_size = Decimal::try_from(config.max_order_size_usdc).unwrap_or(dec!(100.0));
        let order_size = opp.yes_size.min(opp.no_size).min(max_order_size);
        let yes_cost = opp.yes_ask_price * order_size;
        let no_cost = opp.no_ask_price * order_size;
        let total_cost = yes_cost + no_cost;

        let pt = ctx.position_tracker;
        let current_exposure = pt.calculate_exposure();
        if pt.would_exceed_limit(yes_cost, no_cost) {
            return vec![Action::Skip {
                reason: format!(
                    "Risk limit | exposure:{:.2} order:{:.2} limit:{:.2}",
                    current_exposure, total_cost, pt.max_exposure()
                ),
            }];
        }

        // ── position balance check ─────────────────────────────
        if ctx.position_balancer.should_skip_arbitrage(opp.yes_token_id, opp.no_token_id) {
            return vec![Action::Skip {
                reason: format!("Position unbalanced | {}", market_display),
            }];
        }

        // ── trade interval check ───────────────────────────────
        let now = Instant::now();
        if let Some(last) = self.last_trade_time {
            if now.saturating_duration_since(last) < MIN_TRADE_INTERVAL {
                return vec![Action::Skip {
                    reason: "Trade interval <3s".to_string(),
                }];
            }
        }
        self.last_trade_time = Some(now);

        // ── all checks passed → emit action ────────────────────
        info!(
            "⚡ Executing arbitrage | {} | profit:{:.2}% | size:{} | cost:{:.2} USD | exposure:{:.2} USD",
            market_display, opp.profit_percentage, order_size, total_cost, current_exposure
        );

        vec![Action::PlaceArbPair {
            opp,
            yes_dir: ctx.yes_dir.to_string(),
            no_dir: ctx.no_dir.to_string(),
        }]
    }
}
