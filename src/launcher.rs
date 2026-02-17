//! Interactive multi-strategy launcher.
//!
//! At startup shows an interactive menu to pick 1-3 strategies,
//! assign a wallet to each, and then runs them all concurrently
//! as independent tokio tasks.
//!
//! Wallets are defined in `.env` as `WALLET_1_KEY` / `WALLET_1_PROXY` / `WALLET_1_NAME`.
//! Falls back to single-mode when no multi-wallet config is detected.

use anyhow::Result;
use std::io::{self, Write};
use tracing::{info, error};

use crate::config::Config;
use crate::strategy::TradingMode;

/// A wallet profile loaded from numbered env vars.
#[derive(Debug, Clone)]
pub struct WalletProfile {
    pub name: String,
    pub private_key: String,
    pub proxy_address: Option<polymarket_client_sdk::types::Address>,
}

/// A configured slot: strategy + wallet.
#[derive(Debug, Clone)]
pub struct Slot {
    pub index: usize,
    pub strategy: TradingMode,
    pub wallet: WalletProfile,
}

/// Load available wallet profiles from env vars (WALLET_1_KEY, WALLET_2_KEY, WALLET_3_KEY).
pub fn load_wallets() -> Vec<WalletProfile> {
    let mut wallets = Vec::new();
    for i in 1..=3 {
        let key_var = format!("WALLET_{}_KEY", i);
        if let Ok(key) = std::env::var(&key_var) {
            if key.trim().is_empty() { continue; }
            let name = std::env::var(format!("WALLET_{}_NAME", i))
                .unwrap_or_else(|_| format!("Wallet-{}", i));
            let proxy = std::env::var(format!("WALLET_{}_PROXY", i))
                .ok()
                .and_then(|s| {
                    let s = s.trim().to_string();
                    if s.is_empty() { None } else { s.parse().ok() }
                });
            wallets.push(WalletProfile {
                name,
                private_key: key.trim().to_string(),
                proxy_address: proxy,
            });
        }
    }
    wallets
}

/// Check if multi-mode is available (at least 1 numbered wallet configured).
pub fn is_multi_mode_available() -> bool {
    !load_wallets().is_empty()
}

/// Interactive prompt: pick strategies and wallets, return slots.
pub fn interactive_pick(wallets: &[WalletProfile]) -> Result<Vec<Slot>> {
    println!();
    println!("╔══════════════════════════════════════════════╗");
    println!("║   🚀 Polymarket Multi-Strategy Launcher      ║");
    println!("╚══════════════════════════════════════════════╝");
    println!();

    // Show available strategies
    println!("Available strategies:");
    println!("  1. arb  — Spread arbitrage (YES+NO < 1.0)");
    println!("  2. mm   — Adversarial Market Maker");
    println!();

    // Show available wallets
    println!("Available wallets:");
    for (i, w) in wallets.iter().enumerate() {
        let addr_preview = w.proxy_address
            .map(|a| format!("{}", a))
            .unwrap_or_else(|| "no proxy".to_string());
        let key_preview = if w.private_key.len() > 10 {
            format!("{}...{}", &w.private_key[..6], &w.private_key[w.private_key.len()-4..])
        } else {
            "***".to_string()
        };
        println!("  {}. {} (key: {} | proxy: {})", i + 1, w.name, key_preview, addr_preview);
    }
    println!();

    // Ask how many strategies
    let num_slots = prompt_number(
        "How many strategies to run? (1-3)",
        1,
        3.min(wallets.len()),
    )?;

    let mut slots = Vec::new();
    let mut used_wallets: Vec<usize> = Vec::new();

    for slot_idx in 0..num_slots {
        println!();
        println!("── Slot {} ──", slot_idx + 1);

        // Pick strategy
        let mode = prompt_strategy()?;

        // Pick wallet (exclude already used)
        let wallet_idx = prompt_wallet(wallets, &used_wallets)?;
        used_wallets.push(wallet_idx);

        slots.push(Slot {
            index: slot_idx + 1,
            strategy: mode,
            wallet: wallets[wallet_idx].clone(),
        });
    }

    println!();
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!("  Configuration summary:");
    for slot in &slots {
        println!("  Slot {} | strategy:{} | wallet:{} | dry_run:{}",
            slot.index,
            slot.strategy,
            slot.wallet.name,
            std::env::var("DRY_RUN").unwrap_or_else(|_| "false".to_string()),
        );
    }
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    let confirm = prompt_line("Start? (y/n)")?;
    if !confirm.trim().eq_ignore_ascii_case("y") {
        anyhow::bail!("Aborted by user");
    }

    Ok(slots)
}

/// Build slots from env without interactive prompts.
/// Strategy precedence per wallet:
/// 1) WALLET_N_STRATEGY (arb/mm/hybrid)
/// 2) fallback to global TRADING_MODE
pub fn build_slots_non_interactive(
    wallets: &[WalletProfile],
    default_strategy: TradingMode,
) -> Result<Vec<Slot>> {
    let mut slots = Vec::new();

    for (i, wallet) in wallets.iter().enumerate() {
        let idx = i + 1;
        let strategy_var = format!("WALLET_{}_STRATEGY", idx);
        let strategy = match std::env::var(&strategy_var) {
            Ok(raw) if !raw.trim().is_empty() => match raw.trim().to_lowercase().as_str() {
                "arb" => TradingMode::Arb,
                "mm" | "market_maker" | "marketmaker" => TradingMode::MarketMaker,
                "hybrid" => TradingMode::Hybrid,
                _ => anyhow::bail!(
                    "{} has invalid value '{}'. Use one of: arb, mm, hybrid",
                    strategy_var,
                    raw
                ),
            },
            _ => default_strategy.clone(),
        };

        slots.push(Slot {
            index: idx,
            strategy,
            wallet: wallet.clone(),
        });
    }

    Ok(slots)
}

/// Run multiple slots concurrently.
/// Each slot gets its own Config with the selected wallet + strategy.
pub async fn run_slots(base_config: Config, slots: Vec<Slot>) -> Result<()> {
    let mut handles = Vec::new();

    for slot in slots {
        // Clone base config and override wallet + mode
        let mut slot_config = base_config.clone();
        slot_config.private_key = slot.wallet.private_key.clone();
        slot_config.proxy_address = slot.wallet.proxy_address;
        slot_config.trading_mode = slot.strategy.clone();

        let slot_name = format!("Slot-{}/{}/{}", slot.index, slot.strategy, slot.wallet.name);
        info!("🚀 Starting {} ...", slot_name);

        handles.push(tokio::spawn(async move {
            if let Err(e) = crate::run_trading_loop(slot_config, &slot_name).await {
                error!("[{}] Trading loop exited with error: {}", slot_name, e);
            }
        }));
    }

    // Wait for all slots (they run forever until error/ctrl-c)
    for handle in handles {
        let _ = handle.await;
    }

    Ok(())
}

// ── Prompt helpers (stdin) ─────────────────────────────────────

fn prompt_line(prompt: &str) -> Result<String> {
    print!("{}: ", prompt);
    io::stdout().flush()?;
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

fn prompt_number(prompt: &str, min: usize, max: usize) -> Result<usize> {
    loop {
        let input = prompt_line(prompt)?;
        if let Ok(n) = input.parse::<usize>() {
            if n >= min && n <= max {
                return Ok(n);
            }
        }
        println!("  Please enter a number between {} and {}", min, max);
    }
}

pub fn prompt_strategy() -> Result<TradingMode> {
    loop {
        let input = prompt_line("Strategy [arb/mm]")?;
        match input.to_lowercase().as_str() {
            "arb" | "1" => return Ok(TradingMode::Arb),
            "mm" | "2" => return Ok(TradingMode::MarketMaker),
            _ => println!("  Please enter 'arb' or 'mm'"),
        }
    }
}

fn prompt_wallet(wallets: &[WalletProfile], used: &[usize]) -> Result<usize> {
    // Show available (not yet used)
    let available: Vec<usize> = (0..wallets.len())
        .filter(|i| !used.contains(i))
        .collect();

    if available.len() == 1 {
        println!("  Auto-selecting wallet: {} (only one available)", wallets[available[0]].name);
        return Ok(available[0]);
    }

    println!("  Available wallets:");
    for &i in &available {
        println!("    {}. {}", i + 1, wallets[i].name);
    }

    loop {
        let input = prompt_line("  Wallet number")?;
        if let Ok(n) = input.parse::<usize>() {
            let idx = n.saturating_sub(1);
            if available.contains(&idx) {
                return Ok(idx);
            }
        }
        println!("    Please enter a valid wallet number");
    }
}
