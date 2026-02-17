//! Combined Strategy: "The Apex Market Maker"
//!
//! Features:
//! 1) phase-based market making (4 phases)
//! 2) flow-toxicity adaptation
//! 3) inventory skew/rebalancing (Avellaneda-Stoikov)
//! 4) near-expiry resolution handling

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tracing::{info, warn};

use crate::monitor::orderbook::OrderBookPair;
use super::{Action, StrategyContext};

// ───────────────────────────────────────────────────────────────────
// Configuration
// ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ApexConfig {
    pub base_spread: Decimal,
    pub inventory_gamma: Decimal,
    pub max_position_per_side: Decimal,
    pub quote_size: Decimal,
    pub min_seconds_to_quote: i64,
    /// Toxicity threshold: quotes per 10 seconds
    pub toxicity_quote_threshold: u32,
    /// Toxicity threshold: OB imbalance shift percentage (0.1 = 10%)
    pub toxicity_imbalance_threshold: Decimal,
}

impl Default for ApexConfig {
    fn default() -> Self {
        Self {
            base_spread: dec!(0.03),
            inventory_gamma: dec!(0.05),
            max_position_per_side: dec!(10),
            quote_size: dec!(1),
            min_seconds_to_quote: 10,
            toxicity_quote_threshold: 5,
            toxicity_imbalance_threshold: dec!(0.3),
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// Time-decay phases
// ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ApexPhase {
    /// Phase 1 (0-60s): wide quotes
    Early,
    /// Phase 2 (60-180s): adaptive quotes with toxicity widening/tightening
    Adaptive,
    /// Phase 3 (180-240s): inventory cleanup bias
    Cleanup,
    /// Phase 4 (240-300s): near-expiry resolution behavior
    Resolution,
}

impl ApexPhase {
    fn from_elapsed_secs(elapsed: i64) -> Self {
        if elapsed < 60 {
            ApexPhase::Early
        } else if elapsed < 180 {
            ApexPhase::Adaptive
        } else if elapsed < 240 {
            ApexPhase::Cleanup
        } else {
            ApexPhase::Resolution
        }
    }

    fn spread_multiplier(&self) -> Decimal {
        match self {
            ApexPhase::Early => dec!(1.5),
            ApexPhase::Adaptive => dec!(1.0),
            ApexPhase::Cleanup => dec!(1.2),
            ApexPhase::Resolution => dec!(2.0),
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// Toxicity State
// ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ToxicityState {
    Normal,
    Caution,
    Toxic,
}

impl ToxicityState {
    fn spread_multiplier(&self) -> Decimal {
        match self {
            ToxicityState::Normal => dec!(1.0),
            ToxicityState::Caution => dec!(1.5),
            ToxicityState::Toxic => dec!(3.0),
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// Market State
// ───────────────────────────────────────────────────────────────────

#[derive(Debug)]
struct MarketState {
    /// Pessimistic YES exposure
    yes_held: Decimal,
    /// Pessimistic NO exposure
    no_held: Decimal,
    
    /// Quote velocity tracking
    recent_quotes: Vec<Instant>,
    
    /// OB Imbalance tracking
    last_imbalance: Option<Decimal>,
    toxicity: ToxicityState,
    toxicity_reason: String,
}

impl Default for MarketState {
    fn default() -> Self {
        Self {
            yes_held: dec!(0),
            no_held: dec!(0),
            recent_quotes: Vec::new(),
            last_imbalance: None,
            toxicity: ToxicityState::Normal,
            toxicity_reason: "None".into(),
        }
    }
}

impl MarketState {
    fn net_inventory(&self) -> Decimal {
        self.yes_held - self.no_held
    }

    fn record_quote(&mut self, is_yes: bool, size: Decimal) {
        if is_yes {
            self.yes_held += size;
        } else {
            self.no_held += size;
        }
        
        // Toxicity design rationale: We count unique requote 'bursts' (events) 
        // rather than individual token quotes. Posting YES and NO together 
        // in one cycle is normal, not toxic. Toxic flow is characterized by
        // rapid, abnormal requoting beyond the 2s loop or extreme imbalance shifts.
        let now = Instant::now();
        let should_record = match self.recent_quotes.last() {
            Some(&last) => now.duration_since(last).as_millis() > 500,
            None => true,
        };
        if should_record {
            self.recent_quotes.push(now);
        }
    }

    fn update_toxicity(&mut self, config: &ApexConfig, current_imbalance: Option<Decimal>) {
        let now = Instant::now();
        // Clean up old quotes (> 10s)
        self.recent_quotes.retain(|&t| now.duration_since(t).as_secs() < 10);
        
        let quote_velocity = self.recent_quotes.len() as u32;
        // Keep normal cadence in Normal state; caution should mean "near toxic".
        // Example: threshold=5 => caution starts at 4 (not 2).
        let caution_threshold = config.toxicity_quote_threshold.saturating_sub(1).max(1);
        let min_requote_gap_ms = self
            .recent_quotes
            .windows(2)
            .map(|w| w[1].duration_since(w[0]).as_millis())
            .min();
        let has_rapid_requotes = min_requote_gap_ms.map_or(false, |ms| ms < 1500);
        
        let mut new_toxicity = ToxicityState::Normal;
        let mut reason = String::from("Normal");

        // 1. Velocity Signal
        // Guard with rapid cadence so steady 2s loops are not misclassified as toxic flow.
        if has_rapid_requotes && quote_velocity >= config.toxicity_quote_threshold {
            new_toxicity = ToxicityState::Toxic;
            reason = format!(
                "High velocity: {} quotes/10s (min gap: {}ms)",
                quote_velocity,
                min_requote_gap_ms.unwrap_or(0)
            );
        } else if has_rapid_requotes && quote_velocity >= caution_threshold {
            new_toxicity = ToxicityState::Caution;
            reason = format!(
                "Moderate velocity: {} quotes/10s (min gap: {}ms)",
                quote_velocity,
                min_requote_gap_ms.unwrap_or(0)
            );
        }

        // 2. Imbalance Shift Signal
        if let (Some(curr), Some(prev)) = (current_imbalance, self.last_imbalance) {
            let shift = (curr - prev).abs();
            if shift >= config.toxicity_imbalance_threshold {
                if new_toxicity != ToxicityState::Toxic {
                    new_toxicity = ToxicityState::Toxic;
                    reason = format!("OB Imbalance shift: {:.2} -> {:.2}", prev, curr);
                }
            } else if shift >= config.toxicity_imbalance_threshold / dec!(2) {
                if new_toxicity == ToxicityState::Normal {
                    new_toxicity = ToxicityState::Caution;
                    reason = format!("OB Imbalance drift: {:.2} -> {:.2}", prev, curr);
                }
            }
        }
        
        self.last_imbalance = current_imbalance;
        self.toxicity = new_toxicity;
        self.toxicity_reason = reason;
    }

    fn calculate_quotes(
        &self,
        config: &ApexConfig,
        phase: ApexPhase,
        yes_fair: Decimal,
        no_fair: Decimal,
    ) -> (Decimal, Decimal, bool, bool) {
        let net_inv = self.net_inventory();
        
        // 1. Inventory Adjusted Reservation Price (Avellaneda-Stoikov)
        let yes_reservation = yes_fair - config.inventory_gamma * net_inv;
        let no_reservation = no_fair + config.inventory_gamma * net_inv;

        // 2. Spread Calculation
        let mut half_spread = config.base_spread 
            * phase.spread_multiplier() 
            * self.toxicity.spread_multiplier();

        if phase == ApexPhase::Cleanup && net_inv.abs() > dec!(2) {
             half_spread *= dec!(0.8);
        }

        let mut yes_bid = (yes_reservation - half_spread).max(dec!(0.01));
        let mut no_bid = (no_reservation - half_spread).max(dec!(0.01));

        // 3. Position limits & Resolution Phase Logic
        let mut can_bid_yes = self.yes_held < config.max_position_per_side;
        let mut can_bid_no = self.no_held < config.max_position_per_side;

        if phase == ApexPhase::Resolution {
            if net_inv > dec!(5) {
                can_bid_yes = false; 
                let no_cap = (no_fair - dec!(0.005)).max(dec!(0.01));
                no_bid = (no_bid + dec!(0.02)).min(no_cap).max(dec!(0.01));
            } else if net_inv < dec!(-5) {
                can_bid_no = false;
                let yes_cap = (yes_fair - dec!(0.005)).max(dec!(0.01));
                yes_bid = (yes_bid + dec!(0.02)).min(yes_cap).max(dec!(0.01));
            } else if net_inv > dec!(2) {
                yes_bid = (yes_bid - dec!(0.03)).max(dec!(0.01));
            } else if net_inv < dec!(-2) {
                no_bid = (no_bid - dec!(0.03)).max(dec!(0.01));
            }
        }

        (yes_bid, no_bid, can_bid_yes, can_bid_no)
    }

    fn is_safe_pair_quote(yes_bid: Decimal, no_bid: Decimal, can_bid_yes: bool, can_bid_no: bool) -> bool {
        // Pair-edge invariant is required only when quoting both sides.
        !(can_bid_yes && can_bid_no && (yes_bid + no_bid) >= dec!(0.99))
    }
}

// ───────────────────────────────────────────────────────────────────
// ApexMarketMakerStrategy
// ───────────────────────────────────────────────────────────────────

pub struct ApexMarketMakerStrategy {
    config: ApexConfig,
    markets: HashMap<String, MarketState>,
    markets_with_quotes: HashSet<String>,
    window_start: Option<DateTime<Utc>>,
    last_requote: Option<Instant>,
}

impl ApexMarketMakerStrategy {
    pub fn new(config: ApexConfig) -> Self {
        Self {
            config,
            markets: HashMap::new(),
            markets_with_quotes: HashSet::new(),
            window_start: None,
            last_requote: None,
        }
    }

    pub fn name(&self) -> &str { "apex_mm" }

    pub fn on_window_start(&mut self) {
        self.markets.clear();
        self.markets_with_quotes.clear();
        self.window_start = Some(Utc::now());
        self.last_requote = None;
        info!("💎 Apex MM: New window started");
    }

    pub async fn on_book_update(
        &mut self,
        pair: &OrderBookPair,
        ctx: &StrategyContext<'_>,
    ) -> Vec<Action> {
        let mut actions = Vec::new();

        // 1. Time & Phase
        let window_start = match self.window_start {
            Some(ws) => ws,
            None => return vec![Action::Skip { reason: "Window not started".into() }],
        };
        let now = Utc::now();
        let elapsed_secs = now.signed_duration_since(window_start).num_seconds();
        let secs_remaining = ctx.window_end.signed_duration_since(now).num_seconds();
        let phase = ApexPhase::from_elapsed_secs(elapsed_secs);

        if secs_remaining < self.config.min_seconds_to_quote {
            return vec![Action::Skip {
                reason: format!("{}s remaining < {}s min", secs_remaining, self.config.min_seconds_to_quote),
            }];
        }

        // 2. Rate Limit
        if let Some(last) = self.last_requote {
            if last.elapsed().as_millis() < 2000 {
                return vec![];
            }
        }
        self.last_requote = Some(Instant::now());

        // 3. Fair Value
        let yes_mid = Self::midpoint(&pair.yes_book);
        let no_mid = Self::midpoint(&pair.no_book);
        let (yes_fair, no_fair) = match (yes_mid, no_mid) {
            (Some(y), Some(n)) => {
                let total = y + n;
                if total > dec!(0) {
                    (y / total, n / total)
                } else {
                    return vec![Action::Skip { reason: "Zero midpoint sum".into() }];
                }
            }
            _ => return vec![Action::Skip { reason: "Incomplete book data".into() }],
        };

        // 4. Toxicity & Market State
        let market_key = format!("{}", pair.market_id);
        let state = self.markets.entry(market_key.clone()).or_default();
        
        // Calculate imbalance: (bids - asks) / (bids + asks)
        let yes_imbalance = Self::book_imbalance(&pair.yes_book);
        state.update_toxicity(&self.config, yes_imbalance);

        if state.toxicity == ToxicityState::Toxic {
            warn!("🛑 Toxic Market [{}] | Reason: {}", market_key, state.toxicity_reason);
            // Cancel quotes and skip
            if self.markets_with_quotes.contains(&market_key) {
                self.markets_with_quotes.clear();
                return vec![Action::CancelAll, Action::Skip { reason: format!("Toxic: {}", state.toxicity_reason) }];
            }
            return vec![Action::Skip { reason: format!("Toxic: {}", state.toxicity_reason) }];
        }

        // 5. Calculate Quotes
        let (yes_bid, no_bid, can_bid_yes, can_bid_no) = state.calculate_quotes(
            &self.config,
            phase,
            yes_fair,
            no_fair,
        );

        // 6. Quote Safety
        if !MarketState::is_safe_pair_quote(yes_bid, no_bid, can_bid_yes, can_bid_no) {
            return vec![Action::Skip {
                reason: format!("Unsafe dual-side bid sum: {:.4} (>= 0.99)", yes_bid + no_bid),
            }];
        }

        // 7. Position limits final check
        if !can_bid_yes && !can_bid_no {
            return vec![Action::Skip {
                reason: format!("Limits/Resolution reached | YES:{} NO:{} | inv:{}", state.yes_held, state.no_held, state.net_inventory()),
            }];
        }

        // 10. Execute
        if self.markets_with_quotes.contains(&market_key) {
            actions.push(Action::CancelAll);
            self.markets_with_quotes.clear();
        }

        let edge_pct = (dec!(1) - (yes_bid + no_bid)) * dec!(100);
        let mut posted_any = false;

        if can_bid_yes {
            info!(
                "💎 [Apex] Quote YES | bid:{:.4} | fair:{:.4} | phase:{:?} | tox:{:?} | inv:{} | edge:{:.2}%",
                yes_bid, yes_fair, phase, state.toxicity, state.net_inventory(), edge_pct
            );
            if let Some(mi) = ctx.market_info {
                actions.push(Action::PostLimitOrder {
                    token_id: mi.yes_token_id,
                    price: yes_bid,
                    size: self.config.quote_size,
                });
                state.record_quote(true, self.config.quote_size);
                posted_any = true;
            }
        }

        if can_bid_no {
            info!(
                "💎 [Apex] Quote NO  | bid:{:.4} | fair:{:.4} | phase:{:?} | tox:{:?} | inv:{} | edge:{:.2}%",
                no_bid, no_fair, phase, state.toxicity, state.net_inventory(), edge_pct
            );
            if let Some(mi) = ctx.market_info {
                actions.push(Action::PostLimitOrder {
                    token_id: mi.no_token_id,
                    price: no_bid,
                    size: self.config.quote_size,
                });
                state.record_quote(false, self.config.quote_size);
                posted_any = true;
            }
        }

        if posted_any {
            self.markets_with_quotes.insert(market_key);
        }

        actions
    }

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

    fn book_imbalance(book: &polymarket_client_sdk::clob::ws::types::response::BookUpdate) -> Option<Decimal> {
        let bid_vol: Decimal = book.bids.iter().take(3).map(|b| b.size).sum();
        let ask_vol: Decimal = book.asks.iter().rev().take(3).map(|a| a.size).sum();
        let total = bid_vol + ask_vol;
        if total > dec!(0) {
            Some((bid_vol - ask_vol) / total)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use std::time::Duration;

    #[test]
    fn test_phase_transitions() {
        assert_eq!(ApexPhase::from_elapsed_secs(0), ApexPhase::Early);
        assert_eq!(ApexPhase::from_elapsed_secs(59), ApexPhase::Early);
        assert_eq!(ApexPhase::from_elapsed_secs(60), ApexPhase::Adaptive);
        assert_eq!(ApexPhase::from_elapsed_secs(179), ApexPhase::Adaptive);
        assert_eq!(ApexPhase::from_elapsed_secs(180), ApexPhase::Cleanup);
        assert_eq!(ApexPhase::from_elapsed_secs(239), ApexPhase::Cleanup);
        assert_eq!(ApexPhase::from_elapsed_secs(240), ApexPhase::Resolution);
    }

    #[test]
    fn test_toxicity_burst_protection() {
        let config = ApexConfig::default();
        let mut state = MarketState::default();
        
        // 10 quotes in immediate succession (burst) should only count as 1 event
        // because of the 500ms debounce.
        for _ in 0..10 {
            state.record_quote(true, dec!(1));
        }
        state.update_toxicity(&config, None);
        assert_eq!(state.toxicity, ToxicityState::Normal, "Should not be toxic from a single rapid burst");
    }

    #[test]
    fn test_toxicity_caution_threshold_not_too_low() {
        let config = ApexConfig::default(); // toxicity threshold = 5 => caution should start at 4
        let mut state = MarketState::default();

        // Simulate 2 normal events within 10s; should remain Normal.
        state.recent_quotes.push(Instant::now());
        state.recent_quotes.push(Instant::now());
        state.update_toxicity(&config, None);
        assert_eq!(state.toxicity, ToxicityState::Normal);
    }

    #[test]
    fn test_toxicity_normal_requote_cadence_not_flagged() {
        let config = ApexConfig::default();
        let mut state = MarketState::default();
        let now = Instant::now();

        // 5 events over 10s at ~2s cadence should be treated as normal operation.
        state.recent_quotes = vec![
            now - Duration::from_secs(8),
            now - Duration::from_secs(6),
            now - Duration::from_secs(4),
            now - Duration::from_secs(2),
            now,
        ];
        state.update_toxicity(&config, None);
        assert_eq!(state.toxicity, ToxicityState::Normal);
    }

    #[test]
    fn test_toxicity_imbalance_shift() {
        let config = ApexConfig::default();
        let mut state = MarketState::default();
        
        state.update_toxicity(&config, Some(dec!(0.0))); // First update
        state.update_toxicity(&config, Some(dec!(0.4))); // Shift of 0.4 > 0.3 threshold
        assert_eq!(state.toxicity, ToxicityState::Toxic);
        assert!(state.toxicity_reason.contains("OB Imbalance shift"));
    }

    #[test]
    fn test_inventory_skew_direction() {
        let mut state = MarketState::default();
        state.record_quote(true, dec!(10)); // YES heavy
        assert_eq!(state.net_inventory(), dec!(10));
        
        state.record_quote(false, dec!(5)); // Add NO
        assert_eq!(state.net_inventory(), dec!(5));
    }

    #[test]
    fn test_reservation_price_skew() {
        let fair_yes = dec!(0.5);
        let gamma = dec!(0.05);
        
        // Case 1: Neutral
        let inv_neutral = dec!(0);
        let res_neutral = fair_yes - gamma * inv_neutral;
        assert_eq!(res_neutral, dec!(0.5));
        
        // Case 2: YES heavy (pos inventory) -> Reservation price should go DOWN (bid lower to buy less YES)
        let inv_yes_heavy = dec!(10);
        let res_yes_heavy = fair_yes - gamma * inv_yes_heavy;
        assert_eq!(res_yes_heavy, dec!(0.0)); // 0.5 - 0.05 * 10 = 0
        
        // Case 3: NO heavy (neg inventory) -> Reservation price should go UP (bid higher to buy more YES)
        let inv_no_heavy = dec!(-10);
        let res_no_heavy = fair_yes - gamma * inv_no_heavy;
        assert_eq!(res_no_heavy, dec!(1.0)); // 0.5 - 0.05 * (-10) = 1.0
    }

    #[test]
    fn test_resolution_flattening() {
        let config = ApexConfig::default();
        let mut state = MarketState::default();
        
        // CASE: YES heavy (net_inv = 10) in Resolution phase
        state.yes_held = dec!(10);
        state.no_held = dec!(0);
        
        let (_yes_bid, no_bid, can_bid_yes, can_bid_no) = state.calculate_quotes(
            &config,
            ApexPhase::Resolution,
            dec!(0.5), // yes_fair
            dec!(0.5), // no_fair
        );
        
        assert!(!can_bid_yes, "Should disable YES bidding when heavily YES heavy");
        assert!(can_bid_no, "Should allow NO bidding");
        assert!(no_bid > dec!(0.4), "Should have aggressive NO bid prices");
    }

    #[test]
    fn test_inventory_reservation_logic() {
        let config = ApexConfig::default();
        let mut state = MarketState::default();
        
        state.yes_held = dec!(5);
        state.no_held = dec!(0);
        
        let (yes_bid_heavy, _no_bid_heavy, _, _) = state.calculate_quotes(
            &config,
            ApexPhase::Adaptive,
            dec!(0.5),
            dec!(0.5),
        );

        state.yes_held = dec!(0);
        let (yes_bid_neutral, _, _, _) = state.calculate_quotes(
            &config,
            ApexPhase::Adaptive,
            dec!(0.5),
            dec!(0.5),
        );
        
        assert!(yes_bid_heavy < yes_bid_neutral, "Reservation price should lower YES bid when YES heavy");
    }

    #[test]
    fn test_quote_safety_invariant_dual_side() {
        assert!(!MarketState::is_safe_pair_quote(dec!(0.495), dec!(0.495), true, true));
        assert!(MarketState::is_safe_pair_quote(dec!(0.495), dec!(0.495), true, false));
        assert!(MarketState::is_safe_pair_quote(dec!(0.495), dec!(0.495), false, true));
        assert!(MarketState::is_safe_pair_quote(dec!(0.47), dec!(0.47), true, true));
    }
}
