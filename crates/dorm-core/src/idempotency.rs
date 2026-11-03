//! Idempotency keys (ADR 0032): make a retried request a no-op that returns the
//! first response, and refuse a key reused for a *different* request.
//!
//! An HTTP client that retries a `POST` (a timeout, a lost response) sends the
//! same `Idempotency-Key`. [`begin`] turns that into one of three outcomes: the
//! first call is [`Begin::Fresh`] (do the work, then [`finish`] with the
//! response); a retry after completion is [`Begin::Replay`] (return the stored
//! response, do nothing); a retry while the first is still running is
//! [`Begin::InProgress`]. A key reused with a different request body is
//! [`Error::IdempotencyMismatch`] — the same key must mean the same operation.

use crate::db;
use crate::error::{Error, Result};
use crate::value::Value;

/// The idempotency-record table.
pub const TABLE: &str = "dorm_idempotency";

/// The DDL that creates the table. `IF NOT EXISTS`, so a second run is a no-op.
pub fn migrations() -> Vec<String> {
    vec![format!(
        "CREATE TABLE IF NOT EXISTS {TABLE} (\
           key TEXT PRIMARY KEY, \
           request_hash TEXT NOT NULL, \
           status TEXT NOT NULL DEFAULT 'pending', \
           response JSONB, \
           created_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
           expires_at TIMESTAMPTZ NOT NULL)"
    )]
}

/// What a [`begin`] found for a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Begin {
    /// First time: do the work, then call [`finish`] with the response.
    Fresh,
    /// Already completed: return this stored response and do nothing.
    Replay(serde_json::Value),
    /// A concurrent request with this key is still running; the caller retries
    /// later (a `409`/`425` in HTTP terms).
    InProgress,
}

/// Claim `key` for a request whose body hashes to `request_hash`, valid for
/// `ttl_secs`. See [`Begin`] for the outcomes; a key reused with a different hash
/// is [`Error::IdempotencyMismatch`].
pub async fn begin(key: &str, request_hash: &str, ttl_secs: i64) -> Result<Begin> {
    // Claim the key if it is free. `($3 || ' seconds')::interval` keeps the bound
    // parameter typed as text (the parameter-type-inference trap — ADR 0030).
    let inserted = db::sql(
        &format!(
            "INSERT INTO {TABLE} (key, request_hash, expires_at) \
             VALUES ($1, $2, now() + ($3 || ' seconds')::interval) \
             ON CONFLICT (key) DO NOTHING \
             RETURNING key"
        ),
        &[
            Value::Text(key.to_string()),
            Value::Text(request_hash.to_string()),
            Value::Text(ttl_secs.max(0).to_string()),
        ],
    )
    .await?;
    if !inserted.is_empty() {
        return Ok(Begin::Fresh);
    }

    // The key was taken: inspect it.
    let rows = db::sql(
        &format!("SELECT request_hash, status, response::text FROM {TABLE} WHERE key = $1"),
        &[Value::Text(key.to_string())],
    )
    .await?;
    let row = rows
        .first()
        .ok_or_else(|| Error::Db("the idempotency row vanished".into()))?;
    let stored_hash = row.try_get::<String>(0)?;
    if stored_hash != request_hash {
        return Err(Error::IdempotencyMismatch {
            key: key.to_string(),
        });
    }
    let status = row.try_get::<String>(1)?;
    if status != "done" {
        return Ok(Begin::InProgress);
    }
    let response_text = row.try_get::<String>(2).unwrap_or_else(|_| "null".into());
    let response = serde_json::from_str(&response_text)
        .map_err(|e| Error::Db(format!("stored idempotent response is not JSON: {e}")))?;
    Ok(Begin::Replay(response))
}

/// Store the response for `key` and mark it done, so a later retry [`Begin::Replay`]s
/// it. Call it once the work the [`Begin::Fresh`] authorised has succeeded.
pub async fn finish(key: &str, response: &serde_json::Value) -> Result<()> {
    db::run(
        &format!("UPDATE {TABLE} SET status = 'done', response = $2 WHERE key = $1"),
        &[Value::Text(key.to_string()), Value::Json(response.clone())],
    )
    .await?;
    Ok(())
}

/// Delete expired idempotency records; returns how many. Run on a schedule.
pub async fn purge_expired() -> Result<u64> {
    db::run(
        &format!("DELETE FROM {TABLE} WHERE expires_at <= now()"),
        &[],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_migration_creates_the_table_with_the_key_as_primary() {
        let ddl = migrations().join("\n");
        assert!(ddl.contains(TABLE));
        assert!(ddl.contains("key TEXT PRIMARY KEY"));
    }
}
