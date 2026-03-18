use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use sha2::{Digest, Sha256};

const KEYCHAIN_SERVICE: &str = "com.agentcoffeechat.identity";
const KEYCHAIN_ACCOUNT: &str = "ed25519-private-key";

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

/// Cryptographic identity derived from an Ed25519 keypair.
#[derive(Debug, Clone)]
pub struct Identity {
    /// Full fingerprint: "SHA256:" followed by first 32 hex chars of the
    /// SHA-256 hash of the public key.
    pub fingerprint: String,
    /// Raw public key bytes (32 bytes).
    pub public_key_bytes: [u8; 32],
    /// First 16 hex chars of the fingerprint hash — used as the BLE/mDNS
    /// identifier.
    pub fingerprint_prefix: String,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load the Ed25519 identity from the macOS Keychain, or generate a new one
/// if none exists yet.
///
/// The private key is stored in the Keychain under service
/// `com.agentcoffeechat.identity`, account `ed25519-private-key`.
///
/// The public key is additionally written to `~/.agentcoffeechat/identity.pub`
/// (base64-encoded) for easy inspection.
pub fn get_or_create_identity() -> Result<Identity> {
    // Try to load an existing private key from the Keychain.
    match load_private_key_from_keychain() {
        Ok(signing_key) => {
            let identity = identity_from_signing_key(&signing_key)?;
            Ok(identity)
        }
        Err(_) => {
            // No key found (or Keychain read failed) — generate a fresh one.
            let signing_key = generate_and_store_key()?;
            let identity = identity_from_signing_key(&signing_key)?;
            Ok(identity)
        }
    }
}

/// Check whether an Ed25519 identity already exists in the macOS Keychain.
pub fn identity_exists_in_keychain() -> bool {
    load_private_key_from_keychain().is_ok()
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

/// Attempt to load the 32-byte Ed25519 private key from the macOS Keychain.
fn load_private_key_from_keychain() -> Result<SigningKey> {
    let key_bytes = security_framework::passwords::get_generic_password(
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
    )
    .context("Ed25519 key not found in Keychain")?;

    if key_bytes.len() != 32 {
        anyhow::bail!(
            "Keychain entry has unexpected length {} (expected 32)",
            key_bytes.len()
        );
    }

    let mut buf = [0u8; 32];
    buf.copy_from_slice(&key_bytes);
    Ok(SigningKey::from_bytes(&buf))
}

/// Generate a new Ed25519 keypair, store the private key in the Keychain, and
/// return the signing key.
fn generate_and_store_key() -> Result<SigningKey> {
    let mut csprng = rand::rngs::OsRng;
    let signing_key = SigningKey::generate(&mut csprng);

    // Store private key (raw 32 bytes) in the Keychain.
    security_framework::passwords::set_generic_password(
        KEYCHAIN_SERVICE,
        KEYCHAIN_ACCOUNT,
        signing_key.as_bytes(),
    )
    .context("Failed to store Ed25519 key in Keychain")?;

    // Also persist the public key to disk for easy inspection.
    if let Err(e) = save_public_key_file(&signing_key) {
        eprintln!(
            "[identity] Warning: could not write identity.pub: {}",
            e
        );
    }

    Ok(signing_key)
}

/// Derive an `Identity` from a `SigningKey`.
fn identity_from_signing_key(signing_key: &SigningKey) -> Result<Identity> {
    let verifying_key = signing_key.verifying_key();
    let public_key_bytes: [u8; 32] = verifying_key.to_bytes();

    // SHA-256 of the raw public key bytes.
    let hash = Sha256::digest(&public_key_bytes);
    let hex_hash = hex_encode(&hash);

    // fingerprint = "SHA256:" + first 32 hex chars
    let fingerprint = format!("SHA256:{}", &hex_hash[..32]);

    // fingerprint_prefix = first 16 hex chars
    let fingerprint_prefix = hex_hash[..16].to_string();

    Ok(Identity {
        fingerprint,
        public_key_bytes,
        fingerprint_prefix,
    })
}

/// Write the public key to `~/.agentcoffeechat/identity.pub` as base64.
fn save_public_key_file(signing_key: &SigningKey) -> Result<()> {
    use base64::Engine;

    let verifying_key = signing_key.verifying_key();
    let pub_bytes = verifying_key.to_bytes();

    let dir = dirs::home_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join(".agentcoffeechat");
    std::fs::create_dir_all(&dir)
        .context("Failed to create ~/.agentcoffeechat directory")?;

    let pub_path = dir.join("identity.pub");
    let encoded = base64::engine::general_purpose::STANDARD.encode(pub_bytes);
    std::fs::write(&pub_path, format!("{}\n", encoded))
        .with_context(|| format!("Failed to write {}", pub_path.display()))?;

    Ok(())
}

/// Simple hex encoder (lowercase) to avoid pulling in the `hex` crate.
fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_from_known_key() {
        // Deterministic key for reproducible testing.
        let seed = [42u8; 32];
        let signing_key = SigningKey::from_bytes(&seed);
        let identity = identity_from_signing_key(&signing_key).unwrap();

        // Fingerprint must start with "SHA256:" and have 32 hex chars after.
        assert!(identity.fingerprint.starts_with("SHA256:"));
        let hex_part = &identity.fingerprint["SHA256:".len()..];
        assert_eq!(hex_part.len(), 32);
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));

        // fingerprint_prefix is the first 16 hex chars.
        assert_eq!(identity.fingerprint_prefix.len(), 16);
        assert_eq!(identity.fingerprint_prefix, &hex_part[..16]);

        // public_key_bytes must be 32 bytes.
        assert_eq!(identity.public_key_bytes.len(), 32);
    }

    #[test]
    fn hex_encode_works() {
        assert_eq!(hex_encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(hex_encode(&[0x00, 0xff]), "00ff");
    }

    #[test]
    fn different_keys_produce_different_fingerprints() {
        let key_a = SigningKey::from_bytes(&[1u8; 32]);
        let key_b = SigningKey::from_bytes(&[2u8; 32]);

        let id_a = identity_from_signing_key(&key_a).unwrap();
        let id_b = identity_from_signing_key(&key_b).unwrap();

        assert_ne!(id_a.fingerprint, id_b.fingerprint);
        assert_ne!(id_a.fingerprint_prefix, id_b.fingerprint_prefix);
    }
}
