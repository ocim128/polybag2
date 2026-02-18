use anyhow::Result;
use polymarket_client_sdk::clob::types::{OrderType, SignatureType};
use std::env;
use std::io::ErrorKind;

use polymarket_client_sdk::types::Address;

use crate::strategy::TradingMode;

/// Parse arbitrage order type: GTC, GTD, FOK, FAK, case-insensitive, defaults to GTD for invalid or unknown values.
fn parse_arbitrage_order_type(s: &str) -> OrderType {
    match s.trim().to_uppercase().as_str() {
        "GTC" => OrderType::GTC,
        "GTD" => OrderType::GTD,
        "FOK" => OrderType::FOK,
        "FAK" => OrderType::FAK,
        _ => OrderType::GTD,
    }
}

/// Parse slippage array: comma-separated, e.g., "-0.02,0.0".
/// Index 0 = slippage for up/flat side, 1 = slippage for down side only. When only one value is provided, it applies to both. Default "0,0.01".
fn parse_slippage(s: &str) -> [f64; 2] {
    let parts: Vec<f64> = s
        .split(',')
        .map(|x| x.trim().parse().unwrap_or(0.0))
        .collect();
    match parts.len() {
        0 => [0.0, 0.01],
        1 => [parts[0], parts[0]],
        _ => [parts[0], parts[1]],
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClobSignatureMode {
    Auto,
    Eoa,
    Proxy,
    GnosisSafe,
}

impl ClobSignatureMode {
    fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "eoa" => Self::Eoa,
            "proxy" => Self::Proxy,
            "gnosis_safe" | "gnosissafe" | "safe" => Self::GnosisSafe,
            _ => Self::Auto,
        }
    }

    /// Resolve the configured mode to a concrete [`SignatureType`].
    ///
    /// When `Auto`, we use CREATE2 derivation to detect whether the supplied
    /// proxy address is a Polymarket Proxy wallet (Magic/email) or a Gnosis
    /// Safe (browser wallet). If neither derivation matches we fall back to
    /// `Proxy` which is correct for most Magic-link setups.
    pub fn resolve(
        self,
        eoa_address: Option<Address>,
        proxy_address: Option<Address>,
    ) -> SignatureType {
        match self {
            Self::Auto => {
                match (eoa_address, proxy_address) {
                    (Some(eoa), Some(proxy)) => {
                        // Try Proxy derivation first (Magic/email wallets)
                        if let Some(derived) =
                            polymarket_client_sdk::derive_proxy_wallet(eoa, polymarket_client_sdk::POLYGON)
                        {
                            if derived == proxy {
                                return SignatureType::Proxy;
                            }
                        }
                        // Try GnosisSafe derivation (browser wallets)
                        if let Some(derived) =
                            polymarket_client_sdk::derive_safe_wallet(eoa, polymarket_client_sdk::POLYGON)
                        {
                            if derived == proxy {
                                return SignatureType::GnosisSafe;
                            }
                        }
                        // Neither matched — user may have supplied a custom proxy.
                        // Default to Proxy which is correct for Magic-link setups.
                        tracing::warn!(
                            "Auto signature detection: proxy address {} does not match \
                             derived Proxy or Safe wallet for EOA {}. Defaulting to Proxy. \
                             If you see invalid-signature errors set CLOB_SIGNATURE_TYPE \
                             explicitly (proxy or gnosis_safe).",
                            proxy, eoa,
                        );
                        SignatureType::Proxy
                    }
                    (_, Some(_)) => {
                        // Proxy is set but we have no EOA to derive from — default Proxy
                        SignatureType::Proxy
                    }
                    _ => SignatureType::Eoa,
                }
            }
            Self::Eoa => SignatureType::Eoa,
            Self::Proxy => SignatureType::Proxy,
            Self::GnosisSafe => SignatureType::GnosisSafe,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Eoa => "eoa",
            Self::Proxy => "proxy",
            Self::GnosisSafe => "gnosis_safe",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub private_key: String,
    pub proxy_address: Option<Address>, // Polymarket Proxy address (if using Email/Magic or Browser Wallet login)
    pub min_profit_threshold: f64,
    pub max_order_size_usdc: f64,
    pub crypto_symbols: Vec<String>,
    pub market_refresh_advance_secs: u64,
    pub risk_max_exposure_usdc: f64,
    pub risk_imbalance_threshold: f64,
    pub hedge_take_profit_pct: f64, // Hedge take-profit percentage (e.g., 0.05 means 5%)
    pub hedge_stop_loss_pct: f64,   // Hedge stop-loss percentage (e.g., 0.05 means 5%)
    pub arbitrage_execution_spread: f64, // Arbitrage execution spread: execute arbitrage when yes+no <= 1 - arbitrage_execution_spread
    /// Slippage [first, second]: use second for down side only, first for up and flat. E.g., "-0.02,0.0"
    pub slippage: [f64; 2],
    pub gtd_expiration_secs: u64, // GTD order expiry time (seconds), default 300 seconds (5 minutes); only effective when arbitrage_order_type=GTD
    /// Order type for arbitrage orders: GTC (Good Till Canceled), GTD (with gtd_expiration_secs), FOK (Fill or Kill), FAK (Fill and Kill)
    pub arbitrage_order_type: OrderType,
    /// CLOB signature mode: auto | eoa | proxy | gnosis_safe
    pub clob_signature_mode: ClobSignatureMode,
    pub stop_arbitrage_before_end_minutes: u64, // Stop arbitrage N minutes before market end, default 0 (don't stop)
    /// Scheduled merge interval (minutes), 0 means disabled. CONDITION_ID is obtained from current window markets like order book.
    pub merge_interval_minutes: u64,
    /// YES price threshold: only execute arbitrage when YES price >= this threshold, default 0.0 (no limit)
    pub min_yes_price_threshold: f64,
    /// NO price threshold: only execute arbitrage when NO price >= this threshold, default 0.0 (no limit)
    pub min_no_price_threshold: f64,
    /// Position sync interval (seconds), default 10 seconds (fetch latest positions from API to override local cache)
    pub position_sync_interval_secs: u64,
    /// Position balance check interval (seconds), default 60 seconds
    pub position_balance_interval_secs: u64,
    /// Imbalance threshold, only cancel open orders when position difference >= this threshold, default 2.0
    pub position_balance_threshold: f64,
    /// Minimum total position requirement, only execute balancing when total position >= this value, default 5.0
    pub position_balance_min_total: f64,
    /// Wind down before window end: trigger wind down (cancel orders → Merge → market sell remaining) this many minutes before current 5-minute window ends. 0 = disabled.
    pub wind_down_before_window_end_minutes: u64,
    /// Limit order price for single-leg sell during wind down (aim for quick execution), default 0.01
    pub wind_down_sell_price: f64,
    /// Trading mode: arb (default), mm (market maker), hybrid
    pub trading_mode: TradingMode,
    /// Dry-run mode: log actions without sending orders (safe for testing)
    pub dry_run: bool,
    // === Market Maker settings ===
    /// MM: base half-spread per side (0.03 = 3¢)
    pub mm_base_spread: f64,
    /// MM: Avellaneda-Stoikov inventory risk-aversion γ
    pub mm_inventory_gamma: f64,
    /// MM: max shares to accumulate per side (YES or NO)
    pub mm_max_position_per_side: f64,
    /// MM: size of each individual quote (shares)
    pub mm_quote_size: f64,
    /// MM: stop quoting when fewer than N seconds remain in window
    pub mm_min_seconds_to_quote: i64,
    /// Startup wizard: show interactive picker for strategy in single-mode
    pub startup_wizard: bool,
    /// Live confirmation: if true, require typing LIVE before trading if DRY_RUN=false
    pub live_confirm_required: bool,
    /// Multi-wallet non-interactive mode: skip picker and build slots from env
    pub multi_wallet_non_interactive: bool,

    // === Apex Market Maker settings ===
    /// Apex: base half-spread per side (0.03 = 3¢)
    pub apex_base_spread: f64,
    /// Apex: Avellaneda-Stoikov inventory risk-aversion γ
    pub apex_inventory_gamma: f64,
    /// Apex: max shares to accumulate per side (YES or NO)
    pub apex_max_position_per_side: f64,
    /// Apex: size of each individual quote (shares)
    pub apex_quote_size: f64,
    /// Apex: stop quoting when fewer than N seconds remain in window
    pub apex_min_seconds_to_quote: i64,
    /// Apex: toxicity threshold (quotes per 10s)
    pub apex_toxicity_quote_threshold: u32,
    /// Apex: toxicity imbalance threshold (0.3 = 30%)
    pub apex_toxicity_imbalance_threshold: f64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        match dotenvy::dotenv() {
            Ok(_) => {}
            Err(dotenvy::Error::Io(err)) if err.kind() == ErrorKind::NotFound => {}
            Err(e) => {
                anyhow::bail!(
                    "Failed to parse .env: {}. Ensure every non-comment line is KEY=VALUE and notes start with '#'.",
                    e
                );
            }
        }

        // Parse proxy_address (optional)
        let proxy_address: Option<Address> = env::var("POLYMARKET_PROXY_ADDRESS")
            .ok()
            .and_then(|addr| addr.parse().ok());

        Ok(Config {
            private_key: env::var("POLYMARKET_PRIVATE_KEY")
                .unwrap_or_default(), // empty in multi-wallet mode; launcher overrides per-slot
            proxy_address,
            min_profit_threshold: env::var("MIN_PROFIT_THRESHOLD")
                .unwrap_or_else(|_| "0.001".to_string())
                .parse()
                .unwrap_or(0.001),
            max_order_size_usdc: env::var("MAX_ORDER_SIZE_USDC")
                .unwrap_or_else(|_| "100.0".to_string())
                .parse()
                .unwrap_or(100.0),
            crypto_symbols: env::var("CRYPTO_SYMBOLS")
                .unwrap_or_else(|_| "btc,eth,xrp,sol".to_string())
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .collect(),
            market_refresh_advance_secs: env::var("MARKET_REFRESH_ADVANCE_SECS")
                .unwrap_or_else(|_| "5".to_string())
                .parse()
                .unwrap_or(5),
            risk_max_exposure_usdc: env::var("RISK_MAX_EXPOSURE_USDC")
                .unwrap_or_else(|_| "1000.0".to_string())
                .parse()
                .unwrap_or(1000.0),
            risk_imbalance_threshold: env::var("RISK_IMBALANCE_THRESHOLD")
                .unwrap_or_else(|_| "0.1".to_string())
                .parse()
                .unwrap_or(0.1),
            hedge_take_profit_pct: env::var("HEDGE_TAKE_PROFIT_PCT")
                .unwrap_or_else(|_| "0.05".to_string())
                .parse()
                .unwrap_or(0.05), // Default 5% take-profit
            hedge_stop_loss_pct: env::var("HEDGE_STOP_LOSS_PCT")
                .unwrap_or_else(|_| "0.05".to_string())
                .parse()
                .unwrap_or(0.05), // Default 5% stop-loss
            arbitrage_execution_spread: env::var("ARBITRAGE_EXECUTION_SPREAD")
                .unwrap_or_else(|_| "0.01".to_string())
                .parse()
                .unwrap_or(0.01), // Default 0.01
            slippage: parse_slippage(&env::var("SLIPPAGE").unwrap_or_else(|_| "0,0.01".to_string())),
            gtd_expiration_secs: env::var("GTD_EXPIRATION_SECS")
                .unwrap_or_else(|_| "300".to_string())
                .parse()
                .unwrap_or(300), // Default 300 seconds (5 minutes)
            arbitrage_order_type: parse_arbitrage_order_type(
                &env::var("ARBITRAGE_ORDER_TYPE").unwrap_or_else(|_| "GTD".to_string()),
            ),
            clob_signature_mode: ClobSignatureMode::parse(
                &env::var("CLOB_SIGNATURE_TYPE").unwrap_or_else(|_| "auto".to_string()),
            ),
            stop_arbitrage_before_end_minutes: env::var("STOP_ARBITRAGE_BEFORE_END_MINUTES")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .unwrap_or(0), // Default 0 (don't stop)
            merge_interval_minutes: env::var("MERGE_INTERVAL_MINUTES")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .unwrap_or(0), // 0 = disabled
            min_yes_price_threshold: env::var("MIN_YES_PRICE_THRESHOLD")
                .unwrap_or_else(|_| "0.0".to_string())
                .parse()
                .unwrap_or(0.0), // Default 0.0 (no limit)
            min_no_price_threshold: env::var("MIN_NO_PRICE_THRESHOLD")
                .unwrap_or_else(|_| "0.0".to_string())
                .parse()
                .unwrap_or(0.0), // Default 0.0 (no limit)
            position_sync_interval_secs: env::var("POSITION_SYNC_INTERVAL_SECS")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .unwrap_or(10), // Default 10 seconds
            position_balance_interval_secs: env::var("POSITION_BALANCE_INTERVAL_SECS")
                .unwrap_or_else(|_| "60".to_string())
                .parse()
                .unwrap_or(60), // Default 60 seconds
            position_balance_threshold: env::var("POSITION_BALANCE_THRESHOLD")
                .unwrap_or_else(|_| "2.0".to_string())
                .parse()
                .unwrap_or(2.0), // Default 2.0
            position_balance_min_total: env::var("POSITION_BALANCE_MIN_TOTAL")
                .unwrap_or_else(|_| "5.0".to_string())
                .parse()
                .unwrap_or(5.0), // Default 5.0
            wind_down_before_window_end_minutes: env::var("WIND_DOWN_BEFORE_WINDOW_END_MINUTES")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .unwrap_or(0), // 0 = disabled
            wind_down_sell_price: env::var("WIND_DOWN_SELL_PRICE")
                .unwrap_or_else(|_| "0.01".to_string())
                .parse()
                .unwrap_or(0.01), // Default 0.01
            trading_mode: TradingMode::parse(
                &env::var("TRADING_MODE").unwrap_or_else(|_| "arb".to_string()),
            ),
            dry_run: env::var("DRY_RUN")
                .unwrap_or_else(|_| "false".to_string())
                .trim()
                .eq_ignore_ascii_case("true"),
            // MM settings
            mm_base_spread: env::var("MM_BASE_SPREAD")
                .unwrap_or_else(|_| "0.03".to_string())
                .parse().unwrap_or(0.03),
            mm_inventory_gamma: env::var("MM_INVENTORY_GAMMA")
                .unwrap_or_else(|_| "0.03".to_string())
                .parse().unwrap_or(0.03),
            mm_max_position_per_side: env::var("MM_MAX_POSITION_PER_SIDE")
                .unwrap_or_else(|_| "5".to_string())
                .parse().unwrap_or(5.0),
            mm_quote_size: env::var("MM_QUOTE_SIZE")
                .unwrap_or_else(|_| "1".to_string())
                .parse().unwrap_or(1.0),
            mm_min_seconds_to_quote: env::var("MM_MIN_SECONDS_TO_QUOTE")
                .unwrap_or_else(|_| "30".to_string())
                .parse().unwrap_or(30),
            startup_wizard: env::var("STARTUP_WIZARD")
                .unwrap_or_else(|_| "false".to_string())
                .trim()
                .eq_ignore_ascii_case("true"),
            live_confirm_required: env::var("LIVE_CONFIRM_REQUIRED")
                .unwrap_or_else(|_| "true".to_string())
                .trim()
                .eq_ignore_ascii_case("true"),
            multi_wallet_non_interactive: env::var("MULTI_WALLET_NON_INTERACTIVE")
                .unwrap_or_else(|_| "false".to_string())
                .trim()
                .eq_ignore_ascii_case("true"),

            // Apex MM settings
            apex_base_spread: env::var("APEX_BASE_SPREAD")
                .unwrap_or_else(|_| "0.03".to_string())
                .parse().unwrap_or(0.03),
            apex_inventory_gamma: env::var("APEX_INVENTORY_GAMMA")
                .unwrap_or_else(|_| "0.05".to_string())
                .parse().unwrap_or(0.05),
            apex_max_position_per_side: env::var("APEX_MAX_POSITION_PER_SIDE")
                .unwrap_or_else(|_| "10".to_string())
                .parse().unwrap_or(10.0),
            apex_quote_size: env::var("APEX_QUOTE_SIZE")
                .unwrap_or_else(|_| "1".to_string())
                .parse().unwrap_or(1.0),
            apex_min_seconds_to_quote: env::var("APEX_MIN_SECONDS_TO_QUOTE")
                .unwrap_or_else(|_| "10".to_string())
                .parse().unwrap_or(10),
            apex_toxicity_quote_threshold: env::var("APEX_TOXICITY_QUOTE_THRESHOLD")
                .unwrap_or_else(|_| "5".to_string())
                .parse().unwrap_or(5),
            apex_toxicity_imbalance_threshold: env::var("APEX_TOXICITY_IMBALANCE_THRESHOLD")
                .unwrap_or_else(|_| "0.3".to_string())
                .parse().unwrap_or(0.3),
        })
    }
}
