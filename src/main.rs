mod config;
mod launcher;
mod market;
mod monitor;
mod risk;
mod strategy;
mod trading;
mod utils;

use poly_5min_bot::merge;
use poly_5min_bot::positions::{get_positions_for, Position};

use anyhow::Result;
use dashmap::DashMap;
use futures::StreamExt;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use polymarket_client_sdk::types::{Address, B256, U256};

use crate::config::Config;
use crate::market::{MarketDiscoverer, MarketInfo, MarketScheduler};
use crate::monitor::OrderBookMonitor;
use crate::risk::positions::PositionTracker;
use crate::risk::{HedgeMonitor, PositionBalancer, RiskManager};
use crate::strategy::{StrategyContext, StrategyRunner, TradingMode};
use crate::strategy::action_executor::ActionExecutor;
use crate::trading::TradingExecutor;

/// Filter condition_ids where both YES and NO positions are held; only these markets can be merged; skip single-sided positions.
/// Data API may return outcome_index 0/1 (0=Yes, 1=No) or 1/2 (consistent with CTF index_set), both are supported.
fn condition_ids_with_both_sides(positions: &[Position]) -> Vec<B256> {
    let mut by_condition: HashMap<B256, HashSet<i32>> = HashMap::new();
    for p in positions {
        if p.size <= dec!(0) {
            continue;
        }
        by_condition
            .entry(p.condition_id)
            .or_default()
            .insert(p.outcome_index);
    }
    by_condition
        .into_iter()
        .filter(|(_, indices)| {
            (indices.contains(&0) && indices.contains(&1)) || (indices.contains(&1) && indices.contains(&2))
        })
        .map(|(c, _)| c)
        .collect()
}

/// Build condition_id -> (yes_token_id, no_token_id, merge_amount) from positions, used to reduce exposure after successful merge.
/// Supports outcome_index 0/1 (0=Yes, 1=No) and 1/2 (CTF convention).
fn merge_info_with_both_sides(positions: &[Position]) -> HashMap<B256, (U256, U256, Decimal)> {
    // Group by condition: outcome_index -> (asset, size)
    let mut by_condition: HashMap<B256, HashMap<i32, (U256, Decimal)>> = HashMap::new();
    for p in positions {
        if p.size <= dec!(0) {
            continue;
        }
        by_condition
            .entry(p.condition_id)
            .or_default()
            .insert(p.outcome_index, (p.asset, p.size));
    }
    by_condition
        .into_iter()
        .filter_map(|(c, map)| {
            // Prefer CTF convention 1=Yes, 2=No; otherwise use 0=Yes, 1=No
            if let (Some((yes_token, yes_size)), Some((no_token, no_size))) =
                (map.get(&1).copied(), map.get(&2).copied())
            {
                return Some((c, (yes_token, no_token, yes_size.min(no_size))));
            }
            if let (Some((yes_token, yes_size)), Some((no_token, no_size))) =
                (map.get(&0).copied(), map.get(&1).copied())
            {
                return Some((c, (yes_token, no_token, yes_size.min(no_size))));
            }
            None
        })
        .collect()
}

/// Scheduled merge task that runs every interval_minutes, fetches positions, only merges markets with both YES+NO positions,
/// runs serially with delays between merges, retries once on RPC rate limit. Updates position_tracker after successful merge.
/// Brief delay before first execution to avoid blocking the stream by competing for the same runtime during order book monitoring startup.
async fn run_merge_task(
    interval_minutes: u64,
    proxy: Address,
    private_key: String,
    position_tracker: Arc<PositionTracker>,
    wind_down_in_progress: Arc<AtomicBool>,
) {
    let interval = Duration::from_secs(interval_minutes * 60);
    /// Delay between each merge to reduce RPC bursts
    const DELAY_BETWEEN_MERGES: Duration = Duration::from_secs(30);
    /// Backoff duration when rate limited (slightly longer than "retry in 10s")
    const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(12);
    /// Delay before first execution to let main loop complete order book subscription and enter select!, avoiding merge blocking stream
    const INITIAL_DELAY: Duration = Duration::from_secs(10);

    // Let main loop complete get_markets, create stream, and enter order book monitoring before first merge
    sleep(INITIAL_DELAY).await;

    loop {
        if wind_down_in_progress.load(Ordering::Relaxed) {
            info!("Wind down in progress, skipping this merge cycle");
            sleep(interval).await;
            continue;
        }
        let (condition_ids, merge_info) = match get_positions_for(proxy).await {
            Ok(positions) => (
                condition_ids_with_both_sides(&positions),
                merge_info_with_both_sides(&positions),
            ),
            Err(e) => {
                warn!(error = %e, "❌ Failed to fetch positions, skipping this merge cycle");
                sleep(interval).await;
                continue;
            }
        };

        if condition_ids.is_empty() {
            debug!("🔄 This merge cycle: no markets meet YES+NO dual position requirement");
        } else {
            info!(
                count = condition_ids.len(),
                "🔄 This merge cycle: {} markets meet YES+NO dual position requirement",
                condition_ids.len()
            );
        }

        for (i, &condition_id) in condition_ids.iter().enumerate() {
            // For 2nd and subsequent markets: wait 30s before merge to avoid overlapping with previous on-chain processing
            if i > 0 {
                info!("This merge cycle: waiting 30s before merging next market ({}/{})", i + 1, condition_ids.len());
                sleep(DELAY_BETWEEN_MERGES).await;
            }
            let mut result = merge::merge_max(condition_id, proxy, &private_key, None).await;
            if result.is_err() {
                let msg = result.as_ref().unwrap_err().to_string();
                if msg.contains("rate limit") || msg.contains("retry in") {
                    warn!(condition_id = %condition_id, "⏳ RPC rate limited, retrying once after {}s", RATE_LIMIT_BACKOFF.as_secs());
                    sleep(RATE_LIMIT_BACKOFF).await;
                    result = merge::merge_max(condition_id, proxy, &private_key, None).await;
                }
            }
            match result {
                Ok(tx) => {
                    info!("✅ Merge completed | condition_id={:#x}", condition_id);
                    info!("  📝 tx={}", tx);
                    // Merge success: reduce position and risk exposure (reduce exposure before position to ensure update_exposure_cost reads pre-merge position)
                    if let Some((yes_token, no_token, merge_amt)) = merge_info.get(&condition_id) {
                        position_tracker.update_exposure_cost(*yes_token, dec!(0), -*merge_amt);
                        position_tracker.update_exposure_cost(*no_token, dec!(0), -*merge_amt);
                        position_tracker.update_position(*yes_token, -*merge_amt);
                        position_tracker.update_position(*no_token, -*merge_amt);
                        info!(
                            "💰 Merge exposure reduced | condition_id={:#x} | amount:{}",
                            condition_id, merge_amt
                        );
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("no available shares") {
                        debug!(condition_id = %condition_id, "⏭️ Skipping merge: no available shares");
                    } else {
                        warn!(condition_id = %condition_id, error = %e, "❌ Merge failed");
                    }
                }
            }
            tokio::task::yield_now().await;
        }

        sleep(interval).await;
    }
}

fn print_startup_summary(config: &Config, wallets: &[crate::launcher::WalletProfile]) {
    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   🚀 Polymarket Bot Startup Summary          ║");
    println!("╚══════════════════════════════════════════════╝");
    println!("  Mode:             {}", if wallets.is_empty() { "Single-wallet" } else { "Multi-wallet" });
    println!("  Dry Run:          {}", if config.dry_run { "ENABLED (Simulated)" } else { "DISABLED (Live Trading!)" });
    println!("  Live Confirm:     {}", if config.live_confirm_required { "REQUIRED" } else { "BYPASSED (Automation Mode)" });
    println!(
        "  MW Auto Start:    {}",
        if config.multi_wallet_non_interactive {
            "ENABLED (from env)"
        } else {
            "DISABLED (interactive picker)"
        }
    );
    println!("  Trading Mode(s):  {}", if wallets.is_empty() { config.trading_mode.to_string() } else { "Multi-strategy".to_string() });
    println!("  Symbols:          {}", config.crypto_symbols.join(", "));
    println!("  Wallets:          {}", if wallets.is_empty() { "1 (detected from .env)".to_string() } else { format!("{} detected", wallets.len()) });
    
    if !wallets.is_empty() {
        println!("\nMulti-wallet details:");
        for (i, w) in wallets.iter().enumerate() {
            let proxy_short = w.proxy_address.map(|a| {
                let s = format!("{:?}", a);
                if s.len() > 10 { format!("{}...{}", &s[..6], &s[s.len()-4..]) } else { s }
            }).unwrap_or_else(|| "none".to_string());
            println!("  [{}] Wallet: {:<12} | Proxy: {}", i + 1, w.name, proxy_short);
        }
    }
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
}

fn confirm_live_trading() -> Result<()> {
    println!();
    println!("⚠️  WARNING: LIVE TRADING ENABLED ⚠️");
    println!("You are about to start trading with REAL FUNDS.");
    print!("Type LIVE to continue: ");
    use std::io::{self, Write};
    io::stdout().flush()?;
    let mut buf = String::new();
    if let Err(e) = io::stdin().read_line(&mut buf) {
        return Err(anyhow::anyhow!("Live confirmation required but no interactive stdin is available: {}", e));
    }
    if buf.trim() == "LIVE" {
        info!("Live trading confirmed. Proceeding...");
        Ok(())
    } else {
        anyhow::bail!("Live trading not confirmed. Aborting.")
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logger
    utils::logger::init_logger()?;
    tracing::info!("Polymarket 5-minute arbitrage bot starting");

    // Load configuration
    let mut config = Config::from_env()?;
    tracing::info!("Configuration loaded");

    // Check for multi-wallet mode
    let wallets = launcher::load_wallets();

    // 1. Startup summary
    print_startup_summary(&config, &wallets);

    if !wallets.is_empty() {
        // Multi-wallet mode
        info!("Multi-wallet mode: {} wallets detected", wallets.len());
        let slots = if config.multi_wallet_non_interactive {
            info!("Multi-wallet non-interactive mode enabled, building slots from env");
            launcher::build_slots_non_interactive(&wallets, config.trading_mode.clone())?
        } else {
            launcher::interactive_pick(&wallets)?
        };

        // Summary run plan
        println!("\n🚀 Final run plan:");
        for slot in &slots {
            println!("  Slot {} | strategy:{} | wallet:{} | dry_run:{}",
                slot.index, slot.strategy, slot.wallet.name, config.dry_run);
        }

        // 3. Live safety check
        if !config.dry_run {
            if config.live_confirm_required {
                confirm_live_trading()?;
            } else {
                warn!("LIVE confirmation bypassed by LIVE_CONFIRM_REQUIRED=false");
            }
        }

        launcher::run_slots(config, slots).await?;
    } else {
        // Single-mode (backward compatible)
        info!("Single-mode: using POLYMARKET_PRIVATE_KEY from .env");

        // 2. Beginner wizard toggle
        if config.startup_wizard {
            info!("Enabling startup wizard for strategy selection...");
            config.trading_mode = launcher::prompt_strategy()?;
            info!("Strategy '{}' selected via wizard", config.trading_mode);
        }

        // 3. Live safety check
        if !config.dry_run {
            if config.live_confirm_required {
                confirm_live_trading()?;
            } else {
                warn!("LIVE confirmation bypassed by LIVE_CONFIRM_REQUIRED=false");
            }
        }

        run_trading_loop(config, "main").await?;
    }

    Ok(())
}

/// Core trading loop — runs one strategy with one wallet.
/// Extracted from the original main() so the launcher can spawn multiple instances.
pub async fn run_trading_loop(config: Config, slot_name: &str) -> Result<()> {
    // Validate private key is present (may be empty in multi-wallet mode if not assigned)
    if config.private_key.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "[{}] No private key configured. Set POLYMARKET_PRIVATE_KEY or use multi-wallet mode (WALLET_N_KEY).",
            slot_name
        ));
    }

    // Initialize components
    let _discoverer = MarketDiscoverer::new(config.crypto_symbols.clone());
    let _scheduler = MarketScheduler::new(_discoverer, config.market_refresh_advance_secs);
    
    // Validate private key format
    info!("Validating private key format...");
    use alloy::signers::local::LocalSigner;
    use polymarket_client_sdk::POLYGON;
    use std::str::FromStr;
    
    let _signer_test = LocalSigner::from_str(&config.private_key)
        .map_err(|e| anyhow::anyhow!("Invalid private key format: {}", e))?;
    info!("Private key format validated");

    // Initialize trading executor (requires authentication)
    info!("Initializing trading executor (requires API authentication)...");
    if let Some(ref proxy) = config.proxy_address {
        info!(proxy_address = %proxy, "Using Proxy signature type (Email/Magic or Browser Wallet)");
    } else {
        info!("Using EOA signature type (direct trading)");
    }
    info!("Note: If you see 'Could not create api key' warning, this is normal. SDK will first try to create a new API key, then fall back to derivation on failure. Authentication will still succeed.");
    let executor = match TradingExecutor::new(
        config.private_key.clone(),
        config.max_order_size_usdc,
        config.proxy_address,
        config.slippage,
        config.gtd_expiration_secs,
        config.arbitrage_order_type.clone(),
    ).await {
        Ok(exec) => {
            info!("Trading executor authentication successful (may have used derived API key)");
            Arc::new(exec)
        }
        Err(e) => {
            error!(error = %e, "Trading executor authentication failed! Cannot continue.");
            error!("Please check:");
            error!("  1. POLYMARKET_PRIVATE_KEY environment variable is set correctly");
            error!("  2. Private key format is correct (should be 64-character hex string without 0x prefix)");
            error!("  3. Network connection is normal");
            error!("  4. Polymarket API service is available");
            return Err(anyhow::anyhow!("Authentication failed, program exiting: {}", e));
        }
    };

    // Create CLOB client for risk management (requires authentication)
    info!("Initializing risk management client (requires API authentication)...");
    use alloy::signers::Signer;
    use polymarket_client_sdk::clob::{Client, Config as ClobConfig};
    use polymarket_client_sdk::clob::types::SignatureType;

    let signer_for_risk = LocalSigner::from_str(&config.private_key)?
        .with_chain_id(Some(POLYGON));
    let clob_config = ClobConfig::builder().use_server_time(true).build();
    let mut auth_builder_risk = Client::new("https://clob.polymarket.com", clob_config)?
        .authentication_builder(&signer_for_risk);
    
    // If proxy_address is provided, set funder and signature_type
    if let Some(funder) = config.proxy_address {
        auth_builder_risk = auth_builder_risk
            .funder(funder)
            .signature_type(SignatureType::Proxy);
    }
    
    let clob_client = match auth_builder_risk.authenticate().await {
        Ok(client) => {
            info!("Risk management client authentication successful (may have used derived API key)");
            client
        }
        Err(e) => {
            error!(error = %e, "Risk management client authentication failed! Cannot continue.");
            error!("Please check:");
            error!("  1. POLYMARKET_PRIVATE_KEY environment variable is set correctly");
            error!("  2. Private key format is correct");
            error!("  3. Network connection is normal");
            error!("  4. Polymarket API service is available");
            return Err(anyhow::anyhow!("Authentication failed, program exiting: {}", e));
        }
    };
    
    let _risk_manager = Arc::new(RiskManager::new(clob_client.clone(), &config));
    
    // Create hedge monitor (pass Arc reference to PositionTracker to update risk exposure)
    // Hedge strategy temporarily disabled, but keep hedge_monitor variable for future use
    let position_tracker = _risk_manager.position_tracker();
    let _hedge_monitor = HedgeMonitor::new(
        clob_client.clone(),
        config.private_key.clone(),
        config.proxy_address.clone(),
        position_tracker,
    );

    // Verify authentication actually succeeded - try a simple API call
    info!("Verifying authentication status (via API call test)...");
    match executor.verify_authentication().await {
        Ok(_) => {
            info!("✅ Authentication verification successful, API calls normal");
        }
        Err(e) => {
            error!(error = %e, "❌ Authentication verification failed! Although authenticate() did not error, API call failed.");
            error!("This indicates authentication did not actually succeed, possible reasons:");
            error!("  1. API key creation failed (saw 'Could not create api key' warning)");
            error!("  2. The account for this private key may not be registered on Polymarket");
            error!("  3. Account may be restricted or suspended");
            error!("  4. Network connection issues");
            error!("Program will exit. Please resolve authentication issues before running again.");
            return Err(anyhow::anyhow!("Authentication verification failed: {}", e));
        }
    }

    info!("[{}] ✅ All components initialized, authentication verified", slot_name);

    // RPC health check components (endpoint probing, circuit breaker, metrics)
    let rpc_cfg = rpc_check::CheckConfig::builder()
        .timeout(Duration::from_secs(5))
        .build();
    let _rpc_checker = rpc_check::RpcChecker::new(rpc_cfg);
    let _rpc_circuit = rpc_check::CircuitBreaker::new();
    let _rpc_metrics = rpc_check::Metrics::new();
    let _ = _rpc_checker.validate_endpoint("https://clob.polymarket.com");
    let _ = _rpc_checker.validate_endpoint("https://gamma-api.polymarket.com");

    // Create position balancer
    let position_balancer = Arc::new(PositionBalancer::new(
        clob_client.clone(),
        _risk_manager.position_tracker(),
        &config,
    ));

    // Scheduled position sync task: fetch latest positions from API every N seconds, overwrite local cache
    let position_sync_interval = config.position_sync_interval_secs;
    if position_sync_interval > 0 {
        let position_tracker_sync = _risk_manager.position_tracker();
        tokio::spawn(async move {
            let interval = Duration::from_secs(position_sync_interval);
            loop {
                match position_tracker_sync.sync_from_api().await {
                    Ok(_) => {
                        // Position info already printed in sync_from_api
                    }
                    Err(e) => {
                        warn!(error = %e, "Position sync failed, will retry in next cycle");
                    }
                }
                sleep(interval).await;
            }
        });
        info!(
            interval_secs = position_sync_interval,
            "Scheduled position sync task started, fetching latest positions from API every {} seconds to overwrite local cache",
            position_sync_interval
        );
    } else {
        warn!("POSITION_SYNC_INTERVAL_SECS=0, position sync disabled");
    }

    // Scheduled position balance task: check positions and orders every N seconds, cancel excess orders
    // Note: Balance task will be called in main loop since it needs market mapping
    let balance_interval = config.position_balance_interval_secs;
    if balance_interval > 0 {
        info!(
            interval_secs = balance_interval,
            "Position balance task will execute every {} seconds in main loop",
            balance_interval
        );
    } else {
        info!("Scheduled position balance not enabled (POSITION_BALANCE_INTERVAL_SECS=0)");
    }

    // Wind down in progress flag: scheduled merge will check and skip to avoid competing with wind down merge
    let wind_down_in_progress = Arc::new(AtomicBool::new(false));

    // Create action executor (DRY_RUN support)
    let action_executor = ActionExecutor::new(
        executor.clone(),
        _risk_manager.clone(),
        _risk_manager.position_tracker(),
        config.dry_run,
    );

    // Scheduled Merge: execute merge every N minutes based on positions, only for markets with both YES+NO positions
    let merge_interval = config.merge_interval_minutes;
    if merge_interval > 0 {
        if let Some(proxy) = config.proxy_address {
            let private_key = config.private_key.clone();
            let position_tracker = _risk_manager.position_tracker().clone();
            let wind_down_flag = wind_down_in_progress.clone();
            tokio::spawn(async move {
                run_merge_task(merge_interval, proxy, private_key, position_tracker, wind_down_flag).await;
            });
            info!(
                interval_minutes = merge_interval,
                "Scheduled Merge task started, executing every {} minutes based on positions (YES+NO dual only)",
                merge_interval
            );
        } else {
            warn!("MERGE_INTERVAL_MINUTES={} but POLYMARKET_PROXY_ADDRESS not set, scheduled Merge disabled", merge_interval);
        }
    } else {
        info!("Scheduled Merge not enabled (MERGE_INTERVAL_MINUTES=0). To enable, set MERGE_INTERVAL_MINUTES to a positive number in .env, e.g., 5 or 15");
    }

    // Heartbeat task: log status every 30 seconds
    let hb_slot_name = slot_name.to_string();
    let hb_strategy = config.trading_mode.to_string();
    let hb_dry_run = config.dry_run;
    let hb_position_tracker = _risk_manager.position_tracker().clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        loop {
            interval.tick().await;
            let exposure = hb_position_tracker.calculate_exposure();
            info!(
                "💓 HEARTBEAT | {} | strategy:{} | dry_run:{} | exposure:${:.2}",
                hb_slot_name, hb_strategy, hb_dry_run, exposure
            );
        }
    });

    // Main loop enabled, start monitoring and trading
    #[allow(unreachable_code)]
    loop {
        // Get markets for current window immediately, wait for next window if failed
        let markets = match _scheduler.get_markets_immediately_or_wait().await {
            Ok(markets) => markets,
            Err(e) => {
                error!(error = %e, "Failed to get markets");
                sleep(Duration::from_secs(60)).await;
                continue;
            }
        };

        if markets.is_empty() {
            warn!("No markets found, skipping current window");
            continue;
        }

        // RPC endpoint health check (batch probe before main loop)
        let _ep_validator = rpc_check::EndpointValidator::new();
        let _ = _ep_validator.normalize("https://clob.polymarket.com");
        let _status = _rpc_checker.perform_health_check_sync("https://clob.polymarket.com");
        let _ = _rpc_circuit.status_allows_request(_status);
        if !_rpc_circuit.is_open() {
            _rpc_circuit.record_success();
            _rpc_metrics.record_check(true);
        }

        // New cycle start: reset risk exposure to accumulate from 0 this cycle
        _risk_manager.position_tracker().reset_exposure();

        // Create strategy for this window
        let mut strategy_runner = match config.trading_mode {
            TradingMode::Arb => StrategyRunner::Arb(
                strategy::arb::ArbitrageStrategy::new(config.min_profit_threshold),
            ),
            TradingMode::MarketMaker => {
                use rust_decimal::Decimal;
                let mm_cfg = strategy::mm::MmConfig {
                    base_spread: Decimal::try_from(config.mm_base_spread).unwrap_or(rust_decimal_macros::dec!(0.03)),
                    inventory_gamma: Decimal::try_from(config.mm_inventory_gamma).unwrap_or(rust_decimal_macros::dec!(0.03)),
                    max_position_per_side: Decimal::try_from(config.mm_max_position_per_side).unwrap_or(rust_decimal_macros::dec!(5)),
                    quote_size: Decimal::try_from(config.mm_quote_size).unwrap_or(rust_decimal_macros::dec!(1)),
                    min_seconds_to_quote: config.mm_min_seconds_to_quote,
                };
                info!("🏪 Market Maker config: {:?}", mm_cfg);
                StrategyRunner::MarketMaker(strategy::mm::MarketMakerStrategy::new(mm_cfg))
            }
            TradingMode::ApexMarketMaker => {
                use rust_decimal::Decimal;
                let apex_cfg = strategy::apex::ApexConfig {
                    base_spread: Decimal::try_from(config.apex_base_spread).unwrap_or(rust_decimal_macros::dec!(0.03)),
                    inventory_gamma: Decimal::try_from(config.apex_inventory_gamma).unwrap_or(rust_decimal_macros::dec!(0.05)),
                    max_position_per_side: Decimal::try_from(config.apex_max_position_per_side).unwrap_or(rust_decimal_macros::dec!(10)),
                    quote_size: Decimal::try_from(config.apex_quote_size).unwrap_or(rust_decimal_macros::dec!(1)),
                    min_seconds_to_quote: config.apex_min_seconds_to_quote,
                    toxicity_quote_threshold: config.apex_toxicity_quote_threshold,
                    toxicity_imbalance_threshold: Decimal::try_from(config.apex_toxicity_imbalance_threshold).unwrap_or(rust_decimal_macros::dec!(0.3)),
                };
                info!("💎 Apex Market Maker config: {:?}", apex_cfg);
                StrategyRunner::ApexMarketMaker(strategy::apex::ApexMarketMakerStrategy::new(apex_cfg))
            }
            TradingMode::Hybrid => {
                warn!("Hybrid mode not yet implemented, falling back to arb");
                StrategyRunner::Arb(
                    strategy::arb::ArbitrageStrategy::new(config.min_profit_threshold),
                )
            }
        };
        strategy_runner.on_window_start();
        info!("Strategy '{}' active for this window", strategy_runner.name());

        // Initialize order book monitor
        let mut monitor = OrderBookMonitor::new();

        // Subscribe to all markets
        for market in &markets {
            if let Err(e) = monitor.subscribe_market(market) {
                error!(error = %e, market_id = %market.market_id, "Failed to subscribe to market");
            }
        }

        // Create order book stream
        let mut stream = match monitor.create_orderbook_stream() {
            Ok(stream) => stream,
            Err(e) => {
                error!(error = %e, "Failed to create order book stream");
                continue;
            }
        };

        info!(market_count = markets.len(), "Starting order book monitoring");

        // Record current window timestamp for cycle switching and wind down trigger detection
        use chrono::Utc;
        use crate::market::discoverer::FIVE_MIN_SECS;
        let current_window_timestamp = MarketDiscoverer::calculate_current_window_timestamp(Utc::now());
        let window_end = chrono::DateTime::from_timestamp(current_window_timestamp + FIVE_MIN_SECS, 0)
            .unwrap_or_else(|| Utc::now());
        let mut wind_down_done = false;

        // Create market ID to market info mapping
        let market_map: HashMap<B256, &MarketInfo> = markets.iter()
            .map(|m| (m.market_id, m))
            .collect();

        // Create market mapping (condition_id -> (yes_token_id, no_token_id)) for position balancing
        let market_token_map: HashMap<B256, (U256, U256)> = markets.iter()
            .map(|m| (m.market_id, (m.yes_token_id, m.no_token_id)))
            .collect();

        // Create scheduled position balance timer
        let balance_interval = config.position_balance_interval_secs;
        let mut balance_timer = if balance_interval > 0 {
            let mut timer = tokio::time::interval(Duration::from_secs(balance_interval));
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            timer.tick().await; // Trigger first time immediately
            Some(timer)
        } else {
            None
        };

        // Record last best ask price per market for calculating price direction (only one HashMap read/write, does not affect monitoring performance)
        let last_prices: DashMap<B256, (Decimal, Decimal)> = DashMap::new();

        // Monitor order book updates
        loop {
            // Wind down check: execute once when <= N minutes before window end (don't break, continue monitoring until window end naturally switches via "new window detection" below)
            // Use second-level precision, num_minutes() truncation in 5-minute window may cause missed detection
            if config.wind_down_before_window_end_minutes > 0 && !wind_down_done {
                let now = Utc::now();
                let seconds_until_end = (window_end - now).num_seconds();
                let threshold_seconds = config.wind_down_before_window_end_minutes as i64 * 60;
                if seconds_until_end <= threshold_seconds {
                    info!("🛑 Wind down triggered | {} seconds until window end", seconds_until_end);
                    wind_down_done = true;
                    wind_down_in_progress.store(true, Ordering::Relaxed);

                    // Wind down executed in separate task, does not block order book; 30 seconds between merges per market
                    let executor_wd = executor.clone();
                    let config_wd = config.clone();
                    let risk_manager_wd = _risk_manager.clone();
                    let wind_down_flag = wind_down_in_progress.clone();
                    tokio::spawn(async move {
                        const MERGE_INTERVAL: Duration = Duration::from_secs(30);

                        // 1. Cancel all orders
                        if let Err(e) = executor_wd.cancel_all_orders().await {
                            warn!(error = %e, "Wind down: failed to cancel all orders, continuing with Merge and sell");
                        } else {
                            info!("✅ Wind down: all orders cancelled");
                        }

                        // Wait 10 seconds after cancel before Merge to avoid positions not yet updated on-chain from orders filled just before cancel
                        const DELAY_AFTER_CANCEL: Duration = Duration::from_secs(10);
                        sleep(DELAY_AFTER_CANCEL).await;

                        // 2. Merge dual positions (wait 30 seconds after each market before merging next) and update exposure
                        let position_tracker = risk_manager_wd.position_tracker();
                        let mut did_any_merge = false;
                        if let Some(proxy) = config_wd.proxy_address {
                            match get_positions_for(proxy).await {
                                Ok(positions) => {
                                    let condition_ids = condition_ids_with_both_sides(&positions);
                                    let merge_info = merge_info_with_both_sides(&positions);
                                    let n = condition_ids.len();
                                    for (i, condition_id) in condition_ids.iter().enumerate() {
                                        match merge::merge_max(*condition_id, proxy, &config_wd.private_key, None).await {
                                            Ok(tx) => {
                                                did_any_merge = true;
                                                info!("✅ Wind down: Merge completed | condition_id={:#x} | tx={}", condition_id, tx);
                                                if let Some((yes_token, no_token, merge_amt)) = merge_info.get(condition_id) {
                                                    position_tracker.update_exposure_cost(*yes_token, dec!(0), -*merge_amt);
                                                    position_tracker.update_exposure_cost(*no_token, dec!(0), -*merge_amt);
                                                    position_tracker.update_position(*yes_token, -*merge_amt);
                                                    position_tracker.update_position(*no_token, -*merge_amt);
                                                    info!("💰 Wind down: Merge reduced exposure | condition_id={:#x} | amount:{}", condition_id, merge_amt);
                                                }
                                            }
                                            Err(e) => {
                                                warn!(condition_id = %condition_id, error = %e, "Wind down: Merge failed");
                                            }
                                        }
                                        // Wait 30 seconds after each market merge before processing next, giving time for on-chain processing
                                        if i + 1 < n {
                                            info!("Wind down: Waiting 30 seconds before merging next market");
                                            sleep(MERGE_INTERVAL).await;
                                        }
                                    }
                                }
                                Err(e) => { warn!(error = %e, "Wind down: failed to get positions, skipping Merge"); }
                            }
                        } else {
                            warn!("Wind down: POLYMARKET_PROXY_ADDRESS not configured, skipping Merge");
                        }

                        // If Merge was executed, wait half minute before selling single leg, giving on-chain processing time; skip wait if no Merge
                        if did_any_merge {
                            sleep(MERGE_INTERVAL).await;
                        }

                        // 3. Market sell remaining single leg positions
                        let wind_down_sell_price = Decimal::try_from(config_wd.wind_down_sell_price).unwrap_or(dec!(0.01));
                        if let Some(sell_proxy) = config_wd.proxy_address {
                            match get_positions_for(sell_proxy).await {
                                Ok(positions) => {
                                    for pos in positions.iter().filter(|p| p.size > dec!(0)) {
                                        let size_floor = (pos.size * dec!(100)).floor() / dec!(100);
                                        if size_floor < dec!(0.01) {
                                            debug!(token_id = %pos.asset, size = %pos.size, "Wind down: position too small, skipping sell");
                                            continue;
                                        }
                                        if let Err(e) = executor_wd.sell_at_price(pos.asset, wind_down_sell_price, size_floor).await {
                                            warn!(token_id = %pos.asset, size = %pos.size, error = %e, "Wind down: failed to sell single leg");
                                        } else {
                                            info!("✅ Wind down: sell order placed | token_id={:#x} | amount:{} | price:{:.4}", pos.asset, size_floor, wind_down_sell_price);
                                        }
                                    }
                                }
                                Err(e) => { warn!(error = %e, "Wind down: failed to get positions, skipping sell"); }
                            }
                        } else {
                            warn!("Wind down: no proxy address configured, skipping sell");
                        }

                        info!("🛑 Wind down complete, continuing to monitor until window end");
                        wind_down_flag.store(false, Ordering::Relaxed);
                    });
                }
            }

            tokio::select! {
                // Process order book updates
                book_result = stream.next() => {
                    match book_result {
                        Some(Ok(book)) => {
                            // Then process order book update (book will be moved)
                            if let Some(pair) = monitor.handle_book_update(book) {
                                // Note: last element of asks is the best ask (lowest sell price)
                                let yes_best_ask = pair.yes_book.asks.last().map(|a| (a.price, a.size));
                                let no_best_ask = pair.no_book.asks.last().map(|a| (a.price, a.size));
                                let total_ask_price = yes_best_ask.and_then(|(p, _)| no_best_ask.map(|(np, _)| p + np));

                                let market_id = pair.market_id;
                                // Compare with previous tick to get price direction (↑up ↓down −flat), no arrow on first tick
                                let (yes_dir, no_dir) = match (yes_best_ask, no_best_ask) {
                                    (Some((yp, _)), Some((np, _))) => {
                                        let prev = last_prices.get(&market_id).map(|r| (r.0, r.1));
                                        let (y_dir, n_dir) = prev
                                            .map(|(ly, ln)| (
                                                if yp > ly { "↑" } else if yp < ly { "↓" } else { "−" },
                                                if np > ln { "↑" } else if np < ln { "↓" } else { "−" },
                                            ))
                                            .unwrap_or(("", ""));
                                        last_prices.insert(market_id, (yp, np));
                                        (y_dir, n_dir)
                                    }
                                    _ => ("", ""),
                                };

                                let market_info = market_map.get(&pair.market_id);
                                let market_title = market_info.map(|m| m.title.as_str()).unwrap_or("Unknown market");
                                let market_symbol = market_info.map(|m| m.crypto_symbol.as_str()).unwrap_or("");
                                let market_display = if !market_symbol.is_empty() {
                                    format!("{} prediction market", market_symbol)
                                } else {
                                    market_title.to_string()
                                };

                                let (prefix, spread_info) = total_ask_price
                                    .map(|t| {
                                        if t < dec!(1.0) {
                                            let profit_pct = (dec!(1.0) - t) * dec!(100.0);
                                            ("🚨Arbitrage opportunity", format!("total:{:.4} profit:{:.2}%", t, profit_pct))
                                        } else {
                                            ("📊", format!("total:{:.4} (no arbitrage)", t))
                                        }
                                    })
                                    .unwrap_or_else(|| ("📊", "no data".to_string()));

                                // Price direction arrows only shown during arbitrage opportunities
                                let is_arbitrage = prefix == "🚨Arbitrage opportunity";
                                let yes_info = yes_best_ask
                                    .map(|(p, s)| {
                                        if is_arbitrage && !yes_dir.is_empty() {
                                            format!("Yes:{:.4} size:{} {}", p, s, yes_dir)
                                        } else {
                                            format!("Yes:{:.4} size:{}", p, s)
                                        }
                                    })
                                    .unwrap_or_else(|| "Yes:none".to_string());
                                let no_info = no_best_ask
                                    .map(|(p, s)| {
                                        if is_arbitrage && !no_dir.is_empty() {
                                            format!("No:{:.4} size:{} {}", p, s, no_dir)
                                        } else {
                                            format!("No:{:.4} size:{}", p, s)
                                        }
                                    })
                                    .unwrap_or_else(|| "No:none".to_string());

                                info!(
                                    "{} {} | {} | {} | {}",
                                    prefix,
                                    market_display,
                                    yes_info,
                                    no_info,
                                    spread_info
                                );
                                
                                // Keep original structured log for debugging (optional)
                                debug!(
                                    market_id = %pair.market_id,
                                    yes_token = %pair.yes_book.asset_id,
                                    no_token = %pair.no_book.asset_id,
                                    "Order book pair details"
                                );

                                // ── Strategy dispatch ──────────────────────────────────
                                // Replaces the previous inline arb detection + execution.
                                // Strategy returns Action values; ActionExecutor runs them.
                                if !wind_down_done {
                                    let market_info_ref = market_map.get(&pair.market_id).copied();
                                    let ctx = StrategyContext {
                                        config: &config,
                                        market_info: market_info_ref,
                                        market_display: &market_display,
                                        position_tracker: &_risk_manager.position_tracker(),
                                        position_balancer: &position_balancer,
                                        window_end,
                                        yes_dir,
                                        no_dir,
                                    };
                                    let actions = strategy_runner.on_book_update(&pair, &ctx).await;
                                    action_executor.execute(actions, &market_display).await;
                                }
                            } // if let Some(pair)
                        } // Some(Ok(book))
                        Some(Err(e)) => {
                            error!(error = %e, "Order book update error");
                            // Stream error, recreate stream
                            break;
                        }
                        None => {
                            warn!("Order book stream ended, recreating");
                            break;
                        }
                    }
                }

                // Scheduled position balance task
                _ = async {
                    if let Some(ref mut timer) = balance_timer {
                        timer.tick().await;
                        if let Err(e) = position_balancer.check_and_balance_positions(&market_token_map).await {
                            warn!(error = %e, "Position balance check failed");
                        }
                    } else {
                        futures::future::pending::<()>().await;
                    }
                } => {
                    // Position balance task executed
                }

                // Periodic check: 1) if entered new 5-minute window 2) wind down trigger (5-minute window needs more frequent checks)
                _ = sleep(Duration::from_secs(1)) => {
                    let now = Utc::now();
                    let new_window_timestamp = MarketDiscoverer::calculate_current_window_timestamp(now);

                    // If current window timestamp differs from recorded, new window has been entered
                    if new_window_timestamp != current_window_timestamp {
                        info!(
                            old_window = current_window_timestamp,
                            new_window = new_window_timestamp,
                            "New 5-minute window detected, preparing to cancel old subscriptions and switch to new window"
                        );
                        // Drop stream first to release borrow on monitor, then clean up old subscriptions
                        drop(stream);
                        monitor.clear();
                        break;
                    }
                }
            }
        }

        // monitor will be automatically dropped at loop end, no manual cleanup needed
        info!("Current window monitoring ended, refreshing markets for next round");
    }

    // Unreachable: the loop above runs forever
    #[allow(unreachable_code)]
    Ok(())
}
