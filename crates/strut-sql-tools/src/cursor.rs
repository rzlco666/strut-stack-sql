//! Opaque cursor pagination tokens (R2-01).
//!
//! A cursor is a base64url payload plus an HMAC-SHA256 tag. The payload names
//! the source, the query fingerprint, the ordering key and the last key value
//! seen; it never contains SQL text or parameter values the caller did not
//! already send. Cursors expire after [`CURSOR_TTL_SECS`]; an expired or
//! tampered token is rejected with a structured `CURSOR_EXPIRED` /
//! `CURSOR_INVALID` error instead of silently restarting from page one.
//!
//! The signing key comes from `STRUT_STACK_SQL_CURSOR_KEY` (hex, 32 bytes) when
//! set, otherwise a random per-process key is generated: cursors issued by one
//! process are not accepted by another, which is the correct default for a
//! server that restarts.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CURSOR_TTL_SECS: u64 = 600;
pub const CURSOR_KEY_ENV: &str = "STRUT_STACK_SQL_CURSOR_KEY";
const CURSOR_VERSION: u8 = 1;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CursorError {
    #[error("cursor expired; restart the query without a cursor")]
    Expired,
    #[error("cursor is invalid: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorPayload {
    pub v: u8,
    /// Unix seconds after which the cursor is rejected.
    pub exp: u64,
    pub source: String,
    /// Sha256 of the exact SQL text (hex).
    pub sql_hash: String,
    /// Sha256 of the bound parameter values (hex).
    pub params_hash: String,
    /// Last ordering-key value of the page, as JSON.
    pub after: serde_json::Value,
    /// Ordering direction of the cursor's query.
    pub descending: bool,
    pub page_size: usize,
}

impl CursorPayload {
    pub fn sql_hash(sql: &str) -> String {
        hex(&Sha256::digest(sql.as_bytes()))
    }

    pub fn params_hash(params: &[serde_json::Value]) -> String {
        let mut hasher = Sha256::new();
        for value in params {
            hasher.update(serde_json::to_vec(value).unwrap_or_default());
            hasher.update([0u8]);
        }
        hex(&hasher.finalize())
    }
}

#[derive(Clone)]
pub struct CursorCodec {
    key: [u8; 32],
}

impl std::fmt::Debug for CursorCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CursorCodec(<redacted>)")
    }
}

impl Default for CursorCodec {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorCodec {
    /// Random per-process key, or the operator's key from the environment.
    pub fn new() -> Self {
        if let Some(raw) = std::env::var_os(CURSOR_KEY_ENV) {
            if let Some(key) = parse_hex_key(&raw.to_string_lossy()) {
                return Self { key };
            }
            tracing::warn!(
                env = CURSOR_KEY_ENV,
                "cursor key is not 64 hex characters; using a random process key"
            );
        }
        let mut key = [0u8; 32];
        // Random source without pulling an RNG dependency: hash time + pid +
        // the address of a stack local. Only used to sign short-lived cursors.
        let mut hasher = Sha256::new();
        hasher.update(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
                .to_le_bytes(),
        );
        hasher.update(std::process::id().to_le_bytes());
        let probe = 0u8;
        hasher.update((&probe as *const u8 as usize).to_le_bytes());
        key.copy_from_slice(&hasher.finalize());
        Self { key }
    }

    pub fn with_key(key: [u8; 32]) -> Self {
        Self { key }
    }

    pub fn encode(&self, payload: &CursorPayload) -> String {
        let body = serde_json::to_vec(payload).expect("cursor payload serializes");
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(&body);
        let tag = mac.finalize().into_bytes();
        format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(&body),
            URL_SAFE_NO_PAD.encode(tag)
        )
    }

    pub fn decode(&self, token: &str, now_unix: u64) -> Result<CursorPayload, CursorError> {
        let (body_b64, tag_b64) = token
            .split_once('.')
            .ok_or_else(|| CursorError::Invalid("expected <payload>.<tag>".into()))?;
        let body = URL_SAFE_NO_PAD
            .decode(body_b64)
            .map_err(|e| CursorError::Invalid(format!("payload is not base64url: {e}")))?;
        let tag = URL_SAFE_NO_PAD
            .decode(tag_b64)
            .map_err(|e| CursorError::Invalid(format!("tag is not base64url: {e}")))?;
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.key).expect("HMAC accepts any key length");
        mac.update(&body);
        mac.verify_slice(&tag)
            .map_err(|_| CursorError::Invalid("signature mismatch".into()))?;
        let payload: CursorPayload = serde_json::from_slice(&body)
            .map_err(|e| CursorError::Invalid(format!("payload is not valid JSON: {e}")))?;
        if payload.v != CURSOR_VERSION {
            return Err(CursorError::Invalid(format!(
                "unsupported cursor version {}",
                payload.v
            )));
        }
        if now_unix >= payload.exp {
            return Err(CursorError::Expired);
        }
        Ok(payload)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn issue(
        &self,
        source: &str,
        sql: &str,
        params: &[serde_json::Value],
        after: serde_json::Value,
        descending: bool,
        page_size: usize,
        now_unix: u64,
    ) -> String {
        self.encode(&CursorPayload {
            v: CURSOR_VERSION,
            exp: now_unix + CURSOR_TTL_SECS,
            source: source.to_string(),
            sql_hash: CursorPayload::sql_hash(sql),
            params_hash: CursorPayload::params_hash(params),
            after,
            descending,
            page_size,
        })
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn parse_hex_key(raw: &str) -> Option<[u8; 32]> {
    let raw = raw.trim();
    if raw.len() != 64 || !raw.is_ascii() {
        return None;
    }
    let mut key = [0u8; 32];
    for (i, chunk) in raw.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).ok()?;
        key[i] = u8::from_str_radix(s, 16).ok()?;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn codec() -> CursorCodec {
        CursorCodec::with_key([7u8; 32])
    }

    #[test]
    fn roundtrip_preserves_payload() {
        let token = codec().issue(
            "app",
            "SELECT id FROM t ORDER BY id",
            &[json!(1)],
            json!(42),
            false,
            25,
            1000,
        );
        let payload = codec().decode(&token, 1001).expect("valid cursor");
        assert_eq!(payload.source, "app");
        assert_eq!(payload.after, json!(42));
        assert!(!payload.descending);
        assert_eq!(payload.page_size, 25);
        assert_eq!(payload.exp, 1000 + CURSOR_TTL_SECS);
    }

    #[test]
    fn expired_cursor_is_rejected() {
        let token = codec().issue("app", "SELECT 1", &[], json!(1), false, 10, 1000);
        assert_eq!(
            codec().decode(&token, 1000 + CURSOR_TTL_SECS),
            Err(CursorError::Expired)
        );
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let token = codec().issue("app", "SELECT 1", &[], json!(1), false, 10, 1000);
        let (body, tag) = token.split_once('.').expect("two parts");
        let mut bytes = URL_SAFE_NO_PAD.decode(body).expect("body");
        bytes[0] ^= 0xff;
        let tampered = format!("{}.{}", URL_SAFE_NO_PAD.encode(bytes), tag);
        assert!(matches!(
            codec().decode(&tampered, 1001),
            Err(CursorError::Invalid(_))
        ));
    }

    #[test]
    fn different_key_rejects_signature() {
        let token = codec().issue("app", "SELECT 1", &[], json!(1), false, 10, 1000);
        let other = CursorCodec::with_key([9u8; 32]);
        assert!(matches!(
            other.decode(&token, 1001),
            Err(CursorError::Invalid(_))
        ));
    }

    #[test]
    fn payload_hashes_distinguish_sql_and_params() {
        assert_ne!(
            CursorPayload::sql_hash("SELECT 1"),
            CursorPayload::sql_hash("SELECT 2")
        );
        assert_ne!(
            CursorPayload::params_hash(&[json!(1)]),
            CursorPayload::params_hash(&[json!(2)])
        );
        assert_eq!(
            CursorPayload::params_hash(&[json!(1), json!("a")]),
            CursorPayload::params_hash(&[json!(1), json!("a")])
        );
    }

    #[test]
    fn cursor_is_opaque_base64url_and_contains_no_sql() {
        let token = codec().issue(
            "app",
            "SELECT secret_column FROM t ORDER BY id",
            &[json!("p@ss")],
            json!(5),
            false,
            10,
            1000,
        );
        assert!(!token.contains('+') && !token.contains('/') && !token.contains('='));
        let body = URL_SAFE_NO_PAD
            .decode(token.split_once('.').expect("two parts").0)
            .expect("body");
        let text = String::from_utf8(body).expect("utf8");
        // The signed body necessarily carries the SQL hash, never the text or
        // the parameter value.
        assert!(!text.contains("secret_column"));
        assert!(!text.contains("p@ss"));
    }

    #[test]
    fn env_key_must_be_64_hex_chars() {
        assert_eq!(parse_hex_key(&"ab".repeat(32)), Some([0xab; 32]));
        assert_eq!(parse_hex_key("short"), None);
        assert_eq!(parse_hex_key(&"zz".repeat(32)), None);
    }
}
