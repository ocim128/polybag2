//! Get user current positions (Data API)

use anyhow::{Context, Result};
use polymarket_client_sdk::data::types::request::PositionsRequest;
use polymarket_client_sdk::data::Client;
use polymarket_client_sdk::types::Address;

/// Position structure returned by Data API, re-exported for caller convenience
pub use polymarket_client_sdk::data::types::response::Position;

/// Reads the `POLYMARKET_PROXY_ADDRESS` environment variable, calls the Data API to get current open positions.
///
/// # Environment Variables
///
/// - `POLYMARKET_PROXY_ADDRESS`: Required, Polymarket proxy wallet address (or EOA address)
///
/// # Errors
///
/// - `POLYMARKET_PROXY_ADDRESS` not set
/// - Address format invalid
/// - Data API call failed
///
/// # Example
///
/// ```ignore
/// use poly_15min_bot::positions::{get_positions, Position};
///
/// let positions = get_positions().await?;
/// for p in positions {
///     println!("{}: {} @ {}", p.title, p.size, p.cur_price);
/// }
/// ```
/// Reads `POLYMARKET_PROXY_ADDRESS` from env and fetches positions.
/// Backward-compatible wrapper; multi-wallet callers should use `get_positions_for()`.
pub async fn get_positions() -> Result<Vec<Position>> {
    dotenvy::dotenv().ok();
    let addr = std::env::var("POLYMARKET_PROXY_ADDRESS")
        .context("POLYMARKET_PROXY_ADDRESS not set")?;
    let user: Address = addr
        .parse()
        .context("POLYMARKET_PROXY_ADDRESS format invalid")?;
    get_positions_for(user).await
}

/// Fetch positions for a specific wallet address (multi-wallet safe).
pub async fn get_positions_for(user: Address) -> Result<Vec<Position>> {
    let client = Client::default();
    let req = PositionsRequest::builder().user(user).build();
    client.positions(&req).await.context("Failed to get positions")
}
