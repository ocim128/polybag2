//! Adversarial Market Maker strategy for 5-minute crypto binary options.
//!
//! Provides liquidity by posting BUY bids on **both** YES and NO sides.
//! If both fill: guaranteed profit (yes_bid + no_bid < 1.0).
//! Dynamically skews quotes via Avellaneda-Stoikov inventory management,
//! adapts spreads across three time-decay phases, and detects toxic flow.
//!
//! All config is via `.env` (see `MmConfig`).

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use polymarket_client_sdk::types::U256;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tracing::{debug, info, warn};

use crate::monitor::orderbook::OrderBookPair;
use super::{Action, StrategyContext};

// ───────────────────────────────────────────────────────────────────
// Configuration (loaded from Config in on_book_update via ctx)
// ───────────────────────────────────────────────────────────────────

/// MM-specific config, parsed from the main Config.
#[derive(Debug, Clone)]
pub struct MmConfig {
    /// Base half-spread (each side).  0.03 → bids 3¢ away from fair value.
    pub base_spread: Decimal,
    /// Avellaneda-Stoikov risk-aversion parameter γ.
    /// Higher = more aggressive inventory rebalancing.
    pub inventory_gamma: Decimal,
    /// Maximum shares to accumulate per side (YES or NO).
    pub max_position_per_side: Decimal,
    /// Size of each individual quote (shares).
    pub quote_size: Decimal,
    /// Minimum seconds remaining in window to post new quotes.
    pub min_seconds_to_quote: i64,
}

impl Default for MmConfig {
    fn default() -> Self {
        Self {
            base_spread: dec!(0.03),
            inventory_gamma: dec!(0.03),
            max_position_per_side: dec!(5),
            quote_size: dec!(1),
            min_seconds_to_quote: 30, // stop quoting 30s before expiry
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// Time-decay phases
// ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// 0 – 2 min: cautious, wide spreads
    Early,
    /// 2 – 4 min: tighter spreads, peak quoting
    Mid,
    /// 4 – 5 min: widen spreads, prepare for expiry
    Late,
}

impl Phase {
    fn from_elapsed_secs(elapsed: i64) -> Self {
        if elapsed < 120 {
            Phase::Early
        } else if elapsed < 240 {
            Phase::Mid
        } else {
            Phase::Late
        }
    }

    /// Spread multiplier for this phase.
    fn spread_multiplier(&self) -> Decimal {
        match self {
            Phase::Early => dec!(1.5),  // wide
            Phase::Mid   => dec!(1.0),  // baseline
            Phase::Late  => dec!(2.0),  // extra wide near expiry
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// Inventory state (tracked per market)
//
// IMPORTANT: This is *synthetic* (pessimistic) tracking. We record
// emitted quotes as if they immediately filled. This is intentionally
// conservative: it may cap quoting sooner than necessary, but will
// never allow the bot to exceed position limits.
//
// Real fill-based tracking would require order-status polling or
// websocket fill notifications, which is a future enhancement.
// ───────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct InventoryState {
    /// Pessimistic YES exposure (emitted quotes, not confirmed fills).
    yes_held: Decimal,
    /// Pessimistic NO exposure (emitted quotes, not confirmed fills).
    no_held: Decimal,
    /// Number of YES quotes emitted this window.
    yes_quotes: u32,
    /// Number of NO quotes emitted this window.
    no_quotes: u32,
    /// Rolling quote-velocity (quotes in the last N seconds).
    recent_quote_count: u32,
    recent_quote_window_start: Option<Instant>,
}

impl InventoryState {
    /// Net inventory: positive = YES-heavy, negative = NO-heavy.
    fn net_inventory(&self) -> Decimal {
        self.yes_held - self.no_held
    }

    /// Record an emitted quote (pessimistic: treated as if filled).
    fn record_quote_emitted(&mut self, is_yes: bool, size: Decimal) {
        if is_yes {
            self.yes_held += size;
            self.yes_quotes += 1;
        } else {
            self.no_held += size;
            self.no_quotes += 1;
        }
        let now = Instant::now();

        // Rolling 10-second window
        match self.recent_quote_window_start {
            Some(start) if now.duration_since(start).as_secs() < 10 => {
                self.recent_quote_count += 1;
            }
            _ => {
                self.recent_quote_window_start = Some(now);
                self.recent_quote_count = 1;
            }
        }
    }

    /// True if quote velocity is suspiciously high (possible loop).
    fn is_toxic(&self) -> bool {
        self.recent_quote_count >= 5  // 5+ quotes in 10s
    }
}

// ───────────────────────────────────────────────────────────────────
// MarketMakerStrategy
// ───────────────────────────────────────────────────────────────────

pub struct MarketMakerStrategy {
    mm_config: MmConfig,
    /// Per-market inventory state (keyed by market_id as string).
    inventory: HashMap<String, InventoryState>,
    /// Markets that currently have live quotes on the book.
    /// CancelAll is account-wide, so when triggered for one market,
    /// all markets' flags are cleared.
    markets_with_quotes: HashSet<String>,
    /// When the current window started (set on on_window_start).
    window_start: Option<DateTime<Utc>>,
    /// Last time we requoted (rate-limit to avoid API spam).
    last_requote: Option<Instant>,
}

impl MarketMakerStrategy {
    pub fn new(mm_config: MmConfig) -> Self {
        Self {
            mm_config,
            inventory: HashMap::new(),
            markets_with_quotes: HashSet::new(),
            window_start: None,
            last_requote: None,
        }
    }

    pub fn name(&self) -> &str { "market_maker" }

    pub fn on_window_start(&mut self) {
        self.inventory.clear();
        self.markets_with_quotes.clear();
        self.window_start = Some(Utc::now());
        self.last_requote = None;
        info!("🏪 Market Maker: new window started");
    }

    pub async fn on_book_update(
        &mut self,
        pair: &OrderBookPair,
        ctx: &StrategyContext<'_>,
    ) -> Vec<Action> {
        let config = &self.mm_config;
        let mut actions = Vec::new();

        // ── Time calculations ──────────────────────────────────
        let window_start = match self.window_start {
            Some(ws) => ws,
            None => return vec![Action::Skip { reason: "Window not started".into() }],
        };
        let now = Utc::now();
        let elapsed_secs = now.signed_duration_since(window_start).num_seconds();
        let secs_remaining = ctx.window_end.signed_duration_since(now).num_seconds();
        let phase = Phase::from_elapsed_secs(elapsed_secs);

        // Stop quoting if too close to expiry
        if secs_remaining < config.min_seconds_to_quote {
            return vec![Action::Skip {
                reason: format!("{}s remaining < {}s min_to_quote", secs_remaining, config.min_seconds_to_quote),
            }];
        }

        // ── Rate-limit requotes (min 2s between) ──────────────
        if let Some(last) = self.last_requote {
            if last.elapsed().as_millis() < 2000 {
                return vec![]; // silent skip, too frequent
            }
        }
        self.last_requote = Some(Instant::now());

        // ── Fair value from order book midpoint ────────────────
        let yes_mid = Self::midpoint(&pair.yes_book);
        let no_mid = Self::midpoint(&pair.no_book);
        let (yes_fair, no_fair) = match (yes_mid, no_mid) {
            (Some(y), Some(n)) => {
                // Normalize so YES + NO = 1.0
                let total = y + n;
                if total > dec!(0) {
                    (y / total, n / total)
                } else {
                    return vec![Action::Skip { reason: "Zero midpoint sum".into() }];
                }
            }
            _ => return vec![Action::Skip { reason: "Incomplete book data".into() }],
        };

        // ── Borrow 1: read inventory state (released before mutation) ──
        let market_key = format!("{}", pair.market_id);
        let (net_inv, is_toxic, toxic_count, yes_held, no_held) = {
            let inv = self.inventory.entry(market_key.clone()).or_default();
            (
                inv.net_inventory(),
                inv.is_toxic(),
                inv.recent_quote_count,
                inv.yes_held,
                inv.no_held,
            )
        }; // borrow released here

        // Check toxic flow → widen spreads
        let toxicity_mult = if is_toxic {
            warn!("🔴 Toxic flow detected | quotes/10s:{}", toxic_count);
            dec!(3.0)
        } else {
            dec!(1.0)
        };

        // Inventory-adjusted reservation price
        // r = s - γ * q * σ² * τ (simplified: σ²*τ ≈ 1 for 5-min binary)
        let yes_reservation = yes_fair - config.inventory_gamma * net_inv;
        let no_reservation = no_fair + config.inventory_gamma * net_inv;

        // ── Spread calculation ─────────────────────────────────
        let half_spread = config.base_spread
            * phase.spread_multiplier()
            * toxicity_mult;

        let yes_bid = (yes_reservation - half_spread).max(dec!(0.01));
        let no_bid = (no_reservation - half_spread).max(dec!(0.01));

        // Safety: ensure total bid < 1.0 (guaranteed profit if both fill)
        let total_bid = yes_bid + no_bid;
        if total_bid >= dec!(0.99) {
            return vec![Action::Skip {
                reason: format!("Bid total {:.4} >= 0.99, no edge", total_bid),
            }];
        }

        // ── Position limits (uses conservative inventory) ──────
        let can_bid_yes = yes_held < config.max_position_per_side;
        let can_bid_no = no_held < config.max_position_per_side;

        if !can_bid_yes && !can_bid_no {
            return vec![Action::Skip {
                reason: format!(
                    "Max position reached | YES:{} NO:{} | max:{}",
                    yes_held, no_held, config.max_position_per_side
                ),
            }];
        }

        // ── Cancel stale quotes for THIS market ────────────────
        // Note: CancelAll is account-wide. When triggered for one
        // market, all markets lose their quotes. We clear all flags
        // and each market re-posts on its next book update.
        if self.markets_with_quotes.contains(&market_key) {
            actions.push(Action::CancelAll);
            self.markets_with_quotes.clear(); // global cancel clears all
        }

        // ── Post new quotes ────────────────────────────────────
        let edge_pct = (dec!(1) - total_bid) * dec!(100);
        let mut posted_yes = false;
        let mut posted_no = false;

        if can_bid_yes {
            info!(
                "🏪 MM Quote YES | bid:{:.4} | fair:{:.4} | phase:{:?} | inv:{} | held(pessimistic):{} | edge:{:.2}%",
                yes_bid, yes_fair, phase, net_inv, yes_held, edge_pct
            );
            if let Some(mi) = ctx.market_info {
                actions.push(Action::PostLimitOrder {
                    token_id: mi.yes_token_id,
                    price: yes_bid,
                    size: config.quote_size,
                });
                posted_yes = true;
            }
        }

        if can_bid_no {
            info!(
                "🏪 MM Quote NO  | bid:{:.4} | fair:{:.4} | phase:{:?} | inv:{} | held(pessimistic):{} | edge:{:.2}%",
                no_bid, no_fair, phase, net_inv, no_held, edge_pct
            );
            if let Some(mi) = ctx.market_info {
                actions.push(Action::PostLimitOrder {
                    token_id: mi.no_token_id,
                    price: no_bid,
                    size: config.quote_size,
                });
                posted_no = true;
            }
        }

        // ── Borrow 2: pessimistic inventory (emitted = assumed filled) ──
        // This is conservative: may cap quoting early. Real fill tracking
        // requires order-status polling (future enhancement).
        if posted_yes || posted_no {
            let inv = self.inventory.entry(market_key.clone()).or_default();
            if posted_yes {
                inv.record_quote_emitted(true, config.quote_size);
                debug!("📊 Pessimistic inventory: YES → {}", inv.yes_held);
            }
            if posted_no {
                inv.record_quote_emitted(false, config.quote_size);
                debug!("📊 Pessimistic inventory: NO → {}", inv.no_held);
            }
            self.markets_with_quotes.insert(market_key);
        }

        actions
    }

    // ── Helpers ─────────────────────────────────────────────────

    /// Order book midpoint (average of best bid and best ask).
    fn midpoint(book: &polymarket_client_sdk::clob::ws::types::response::BookUpdate) -> Option<Decimal> {
        let best_bid = book.bids.first().map(|b| b.price);
        let best_ask = book.asks.last().map(|a| a.price);
        match (best_bid, best_ask) {
            (Some(b), Some(a)) => Some((b + a) / dec!(2)),
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            _ => None,
        }
    }
}
