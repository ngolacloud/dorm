//! Opaque, signed keyset cursors (ADR 0024).
//!
//! A cursor carries the boundary values of the last row of a page **and** a
//! fingerprint of the query that produced it (model, ordering, filters), sealed
//! with HMAC-SHA256 over a configurable key. Decoding verifies the signature and
//! that the fingerprint matches the query asking to continue — so a cursor minted
//! for one query is refused by another, and a tampered cursor is rejected rather
//! than applied to the wrong rows. The values travel as bound parameters once the
//! compiler has them; nothing here reaches SQL as text.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chrono::{DateTime, SecondsFormat, Utc};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use std::sync::RwLock;

use crate::error::{Error, Result};
use crate::value::Value;

type HmacSha256 = Hmac<Sha256>;

/// The environment variable the **CLI** reads to configure the cursor signing
/// key. The library never reads the environment: a framework or application
/// passes the key by API with [`set_cursor_key`], and only the `dorm` CLI reads
/// this name and forwards it. Named here as the canonical variable.
pub const CURSOR_KEY_ENV: &str = "DELONIX_ORM_CURSOR_KEY";

/// The well-known development key. Cursors signed with it are forgeable, so it
/// is for local development only, installed explicitly via [`set_dev_cursor_key`].
const DEV_CURSOR_KEY: &[u8] = b"dorm-dev-cursor-key";

/// The process-wide cursor signing key, configured programmatically. `None`
/// until set: there is deliberately **no default outside development**, so a
/// production service that forgets to configure a key gets a typed error rather
/// than silently forgeable cursors.
static CURSOR_KEY: RwLock<Option<Vec<u8>>> = RwLock::new(None);

/// Sets the HMAC signing key for keyset cursors. A framework or application
/// calls this once at startup with a secret from its own configuration — never
/// read from the environment inside a library. The last call wins.
pub fn set_cursor_key(key: impl Into<Vec<u8>>) {
    *CURSOR_KEY.write().expect("cursor key lock poisoned") = Some(key.into());
}

/// Installs the well-known development key, for local use only. The `dorm` CLI
/// calls this when no real key is configured; a production service must instead
/// call [`set_cursor_key`] with a real secret.
pub fn set_dev_cursor_key() {
    set_cursor_key(DEV_CURSOR_KEY.to_vec());
}

/// Whether a cursor signing key has been configured.
pub fn cursor_key_is_configured() -> bool {
    CURSOR_KEY
        .read()
        .expect("cursor key lock poisoned")
        .is_some()
}

fn signing_key() -> Result<Vec<u8>> {
    key_or_error(CURSOR_KEY.read().expect("cursor key lock poisoned").clone())
}

/// The key-resolution rule, pure so it is testable without the global: a
/// configured key passes through; its absence is a typed error, never a silent
/// default.
fn key_or_error(configured: Option<Vec<u8>>) -> Result<Vec<u8>> {
    configured.ok_or_else(|| {
        Error::Config(
            "keyset cursor signing key is not configured; call \
             dorm_core::cursor::set_cursor_key(..) with a secret (or \
             set_dev_cursor_key() for local development)"
                .into(),
        )
    })
}

/// Encode one boundary value as a tagged, self-describing string. Only the types
/// that can be an ordering key are supported; anything else is a usage error.
fn encode_value(v: &Value) -> Result<String> {
    Ok(match v {
        Value::Bool(b) => format!("b:{b}"),
        Value::I32(n) => format!("i:{n}"),
        Value::I64(n) => format!("l:{n}"),
        Value::F64(x) => format!("f:{x}"),
        Value::Text(s) => format!("t:{s}"),
        Value::Decimal(d) => format!("d:{d}"),
        Value::Timestamp(ts) => format!("s:{}", ts.to_rfc3339_opts(SecondsFormat::Micros, true)),
        Value::Uuid(u) => format!("u:{u}"),
        other => {
            return Err(Error::Config(format!(
                "a {other:?} cannot be a keyset cursor boundary"
            )))
        }
    })
}

fn decode_value(s: &str) -> Result<Value> {
    let (tag, rest) = s
        .split_once(':')
        .ok_or_else(|| Error::Config("malformed cursor value".into()))?;
    let bad = || Error::Config("malformed cursor value".into());
    Ok(match tag {
        "b" => Value::Bool(rest.parse().map_err(|_| bad())?),
        "i" => Value::I32(rest.parse().map_err(|_| bad())?),
        "l" => Value::I64(rest.parse().map_err(|_| bad())?),
        "f" => Value::F64(rest.parse().map_err(|_| bad())?),
        "t" => Value::Text(rest.to_string()),
        "d" => Value::Decimal(rest.parse().map_err(|_| bad())?),
        "s" => Value::Timestamp(
            DateTime::parse_from_rfc3339(rest)
                .map_err(|_| bad())?
                .with_timezone(&Utc),
        ),
        "u" => Value::Uuid(rest.parse().map_err(|_| bad())?),
        _ => return Err(bad()),
    })
}

/// The signed payload: the fingerprint and the boundary values, newline-joined.
/// A newline cannot appear in a tagged value (they are single-line), so it is an
/// unambiguous separator.
fn payload(fingerprint: &str, boundary: &[Value]) -> Result<String> {
    let mut lines = vec![format!("fp:{fingerprint}")];
    for v in boundary {
        lines.push(encode_value(v)?);
    }
    Ok(lines.join("\n"))
}

/// Encode a signed cursor for `boundary`, bound to `fingerprint`.
pub fn encode(fingerprint: &str, boundary: &[Value]) -> Result<String> {
    let body = payload(fingerprint, boundary)?;
    let mut mac = HmacSha256::new_from_slice(&signing_key()?).expect("HMAC takes any key length");
    mac.update(body.as_bytes());
    let sig = mac.finalize().into_bytes();
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(body.as_bytes()),
        URL_SAFE_NO_PAD.encode(sig)
    ))
}

/// Decode and verify `token`, returning its boundary values. Fails if the
/// signature is invalid or the embedded fingerprint does not match `fingerprint`
/// (a cursor from a different query).
pub fn decode(fingerprint: &str, token: &str) -> Result<Vec<Value>> {
    let (body_b64, sig_b64) = token
        .split_once('.')
        .ok_or_else(|| Error::Config("cursor is malformed".into()))?;
    let body = URL_SAFE_NO_PAD
        .decode(body_b64)
        .map_err(|_| Error::Config("cursor is malformed".into()))?;
    let sig = URL_SAFE_NO_PAD
        .decode(sig_b64)
        .map_err(|_| Error::Config("cursor is malformed".into()))?;

    let mut mac = HmacSha256::new_from_slice(&signing_key()?).expect("HMAC takes any key length");
    mac.update(&body);
    mac.verify_slice(&sig)
        .map_err(|_| Error::Config("cursor signature is invalid (tampered or wrong key)".into()))?;

    let body = String::from_utf8(body).map_err(|_| Error::Config("cursor is malformed".into()))?;
    let mut lines = body.split('\n');
    let fp_line = lines
        .next()
        .and_then(|l| l.strip_prefix("fp:"))
        .ok_or_else(|| Error::Config("cursor is malformed".into()))?;
    if fp_line != fingerprint {
        return Err(Error::Config(
            "this cursor was minted for a different query".into(),
        ));
    }
    lines.map(decode_value).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use uuid::Uuid;

    fn sample() -> Vec<Value> {
        vec![
            Value::Text("acme".into()),
            Value::I64(42),
            Value::Decimal(Decimal::new(1999, 2)),
            Value::Uuid(Uuid::nil()),
        ]
    }

    /// Every test that mints or decodes a cursor needs a configured key; the
    /// dev key is shared by all of them, so setting it is safe under the
    /// parallel test runner (every call installs the same bytes).
    fn with_dev_key() {
        set_dev_cursor_key();
    }

    #[test]
    fn a_cursor_round_trips_under_its_own_fingerprint() {
        with_dev_key();
        let tok = encode("fp-A", &sample()).unwrap();
        assert_eq!(decode("fp-A", &tok).unwrap(), sample());
    }

    #[test]
    fn a_cursor_from_another_query_is_refused() {
        with_dev_key();
        let tok = encode("fp-A", &sample()).unwrap();
        let err = decode("fp-B", &tok).unwrap_err().to_string();
        assert!(err.contains("different query"), "{err}");
    }

    #[test]
    fn a_tampered_cursor_is_refused() {
        with_dev_key();
        let tok = encode("fp-A", &sample()).unwrap();
        // Flip a character in the body half.
        let (body, sig) = tok.split_once('.').unwrap();
        let mut body = body.to_string();
        let last = body.pop().unwrap();
        body.push(if last == 'A' { 'B' } else { 'A' });
        let forged = format!("{body}.{sig}");
        assert!(decode("fp-A", &forged).is_err());
    }

    #[test]
    fn an_unconfigured_key_is_a_typed_error_not_a_default() {
        // Pure rule, independent of the global: no key -> typed error; a key
        // passes through. This is the production safety guarantee — no silent
        // forgeable default outside development.
        let err = key_or_error(None).unwrap_err().to_string();
        assert!(err.contains("not configured"), "{err}");
        assert!(key_or_error(Some(b"secret".to_vec())).is_ok());
    }

    #[test]
    fn a_timestamp_boundary_survives_the_round_trip() {
        with_dev_key();
        let now = Utc::now();
        let tok = encode("fp", &[Value::Timestamp(now)]).unwrap();
        match decode("fp", &tok).unwrap().as_slice() {
            [Value::Timestamp(back)] => {
                // Microsecond precision is preserved.
                assert_eq!(back.timestamp_micros(), now.timestamp_micros());
            }
            other => panic!("expected one timestamp, got {other:?}"),
        }
    }
}
