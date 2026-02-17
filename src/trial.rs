//! License file authorization: program only runs with a valid license.
//! License is an encrypted expiration timestamp issued by the author; deleting the license will disable the program.

use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Default license filename (placed in program's current working directory or specified via environment variable)
const LICENSE_FILENAME: &str = "license.key";

/// Environment variable: license file path (optional), defaults to license.key in current directory if not set
const LICENSE_PATH_ENV: &str = "POLY_15MIN_BOT_LICENSE";

/// Key derivation seed (only used to derive encryption key; same seed used when generating the license)
const TRIAL_KEY_SEED: &[u8] = b"poly_15min_bot_trial_seed_2025";

/// AES-GCM nonce length (12 bytes)
const NONCE_LEN: usize = 12;

/// Parse license file path: prioritize environment variable, otherwise use license.key in current directory
fn license_file_path() -> PathBuf {
    std::env::var(LICENSE_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(LICENSE_FILENAME))
}

/// Derive 256-bit key from seed (SHA-256)
fn derive_key() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(TRIAL_KEY_SEED);
    let digest = hasher.finalize();
    digest.into()
}

/// Get current Unix timestamp (seconds)
fn now_secs() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .context("System time error")
}

/// Encrypt a u64 timestamp: outputs base64(nonce || ciphertext), ciphertext includes authentication tag to prevent tampering.
fn encrypt_timestamp(ts_secs: u64) -> Result<String> {
    let key = derive_key();
    let cipher = Aes256Gcm::new_from_slice(&key).context("Failed to initialize encryption")?;
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
    let plaintext = ts_secs.to_le_bytes();
    let ciphertext = cipher
        .encrypt(&nonce, plaintext.as_ref())
        .map_err(|e| anyhow::anyhow!("Encryption failed: {}", e))?;
    let mut payload = nonce.to_vec();
    payload.extend_from_slice(&ciphertext);
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &payload,
    ))
}

/// Decrypt license/trial status content, returns u64 timestamp; returns error if decryption fails or tampered.
fn decrypt_timestamp(encoded: &str) -> Result<u64> {
    let payload = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        encoded.trim(),
    )
    .context("Invalid license format (base64 decode failed)")?;
    if payload.len() < NONCE_LEN {
        anyhow::bail!("Invalid or tampered license (data too short)");
    }
    let (nonce_bytes, ciphertext) = payload.split_at(NONCE_LEN);
    let nonce_arr: [u8; NONCE_LEN] = nonce_bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid or tampered license (nonce length abnormal)"))?;
    let nonce = Nonce::from(nonce_arr);
    let key = derive_key();
    let cipher = Aes256Gcm::new_from_slice(&key).context("Failed to initialize decryption")?;
    let plaintext = cipher
        .decrypt(&nonce, ciphertext)
        .map_err(|_| anyhow::anyhow!("Invalid or tampered license (decryption or verification failed)"))?;
    if plaintext.len() != 8 {
        anyhow::bail!("Invalid or tampered license (content length abnormal)");
    }
    let mut bytes: [u8; 8] = [0; 8];
    bytes.copy_from_slice(&plaintext[..8]);
    Ok(u64::from_le_bytes(bytes))
}

/// Generate license string (encrypted expiration timestamp in base64).
/// For author use: use `gen_license` binary or call this function to generate license, write result to file and send to trial user.
pub fn create_license(expiry_secs: u64) -> Result<String> {
    encrypt_timestamp(expiry_secs)
}

/// Verify license file: file must exist and not expired, otherwise returns error.
/// Deleting the license will disable the program.
pub fn check_license() -> Result<()> {
    let path = license_file_path();
    let now = now_secs()?;

    if !path.exists() {
        anyhow::bail!(
            "License file not found. Please place the {} provided by the author in the program's runtime directory, or set the {} environment variable to specify the path.",
            LICENSE_FILENAME,
            LICENSE_PATH_ENV
        );
    }

    let content = fs::read_to_string(&path).context("Failed to read license file")?;
    let expiry_secs = decrypt_timestamp(&content)?;

    if now >= expiry_secs {
        anyhow::bail!(
            "License has expired. Please contact the author to obtain a new license to continue using."
        );
    }

    let remaining_secs = expiry_secs - now;
    tracing::info!(
        remaining_hours = (remaining_secs as f64) / 3600.0,
        "License valid, approximately {:.1} hours remaining",
        (remaining_secs as f64) / 3600.0
    );
    Ok(())
}
