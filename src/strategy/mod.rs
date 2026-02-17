//! Pluggable trading strategy framework.
//!
//! Strategies return `Action` values for the engine to execute.
//! Switch via `TRADING_MODE` in `.env`.

pub mod arb;
pub mod action_executor;
pub mod mm;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use polymarket_client_sdk::types::U256;
use std::sync::Arc;

use crate::config::Config;
use crate::market::MarketInfo;
use crate::monitor::arbitrage::ArbitrageOpportunity;
use crate::monitor::orderbook::OrderBookPair;
use crate::risk::positions::PositionTracker;
use crate::risk::PositionBalancer;

// -------------------------------------------------------------------
// Action – what a strategy asks the engine to do
// -------------------------------------------------------------------

#[derive(Debug)]
pub enum Action {
    /// Buy both YES and NO (classic spread arb).
    PlaceArbPair {
        opp: ArbitrageOpportunity,
        yes_dir: String,
        no_dir: String,
    },
    /// Post a GTC limit BUY order at a specific price (market making).
    PostLimitOrder {
        token_id: U256,
        price: Decimal,
        size: Decimal,
    },
    /// Cancel all open orders.
    CancelAll,
    /// No-op with a reason for the log.
    Skip { reason: String },
}

// -------------------------------------------------------------------
// Context passed to strategies on each book update
// -------------------------------------------------------------------

pub struct StrategyContext<'a> {
    pub config: &'a Config,
    pub market_info: Option<&'a MarketInfo>,
    pub market_display: &'a str,
    pub position_tracker: &'a Arc<PositionTracker>,
    pub position_balancer: &'a Arc<PositionBalancer>,
    pub window_end: DateTime<Utc>,
    pub yes_dir: &'a str,
    pub no_dir: &'a str,
}

// -------------------------------------------------------------------
// Trading mode
// -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum TradingMode {
    Arb,
    MarketMaker,
    Hybrid,
}

impl TradingMode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "mm" | "market_maker" | "marketmaker" => Self::MarketMaker,
            "hybrid" => Self::Hybrid,
            _ => Self::Arb,
        }
    }
}

impl std::fmt::Display for TradingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Arb => write!(f, "arb"),
            Self::MarketMaker => write!(f, "mm"),
            Self::Hybrid => write!(f, "hybrid"),
        }
    }
}

// -------------------------------------------------------------------
// Enum-based strategy dispatch (no async-trait dependency)
// -------------------------------------------------------------------

pub enum StrategyRunner {
    Arb(arb::ArbitrageStrategy),
    MarketMaker(mm::MarketMakerStrategy),
}

impl StrategyRunner {
    pub fn name(&self) -> &str {
        match self {
            Self::Arb(s) => s.name(),
            Self::MarketMaker(s) => s.name(),
        }
    }

    pub fn on_window_start(&mut self) {
        match self {
            Self::Arb(s) => s.on_window_start(),
            Self::MarketMaker(s) => s.on_window_start(),
        }
    }

    pub async fn on_book_update(
        &mut self,
        pair: &OrderBookPair,
        ctx: &StrategyContext<'_>,
    ) -> Vec<Action> {
        match self {
            Self::Arb(s) => s.on_book_update(pair, ctx).await,
            Self::MarketMaker(s) => s.on_book_update(pair, ctx).await,
        }
    }
}
