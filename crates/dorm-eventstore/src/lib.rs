//! A PostgreSQL event store for DORM (ADR 0039, L12).
//!
//! Event sourcing: the source of truth is an **append-only** log of immutable
//! domain events, one stream per aggregate. The current state of an aggregate is
//! a fold over its stream — never updated in place. This crate is the event store
//! proper that ADR 0035 deferred out of the read-model work: optimistic-concurrency
//! append, aggregate snapshots for fast rebuild, and event-version upcasting on
//! load.
//!
//! The four moving parts:
//!
//! - **[`append`]** writes new events at the next version of an aggregate inside a
//!   transaction. It takes an `expected_version`; if the stream has moved on, the
//!   append is refused with [`Error::Conflict`] — the optimistic-concurrency
//!   control that stops two writers from forking a stream. A `UNIQUE
//!   (aggregate_type, aggregate_id, version)` constraint is the ultimate backstop
//!   under concurrency and is mapped back to the same conflict.
//! - **[`load`]** reads an aggregate's stream in version order, ready to fold.
//! - **Snapshots** ([`save_snapshot`], [`load_from_snapshot`]) cache a folded state
//!   at a version so a long-lived aggregate rebuilds from the snapshot plus the
//!   tail of events, not the whole stream.
//! - **Upcasting** ([`Upcasters`]) transforms the payload of an old `event_version`
//!   up to the current shape as events are loaded, so the fold only ever sees the
//!   latest schema — old events on disk stay untouched.
//!
//! Total order across all aggregates is the `global_seq BIGSERIAL`; per-aggregate
//! order — the only order consistency actually hinges on — is the `version`
//! column. See ADR 0039 for why `BIGSERIAL` is chosen over `xid8`.
//!
//! This crate is optional and depends only on `dorm-core`; the core never depends
//! on it, so a project that does not use it has an unchanged dependency graph.

use std::collections::HashMap;

use dorm_core::db;
use dorm_core::error::{Error, Result};
use dorm_core::value::Value;

/// The append-only event log.
pub const EVENTS: &str = "dorm_events";
/// Per-aggregate folded-state snapshots.
pub const SNAPSHOTS: &str = "dorm_snapshots";

/// The DDL that creates the event-store tables. Appended to a migration's
/// statements, exactly as `dorm-outbox`/`dorm-jobs` do their own. `IF NOT EXISTS`
/// throughout, so a second run is a no-op.
pub fn migrations() -> Vec<String> {
    vec![
        format!(
            "CREATE TABLE IF NOT EXISTS {EVENTS} (\
               global_seq BIGSERIAL PRIMARY KEY, \
               aggregate_type TEXT NOT NULL, \
               aggregate_id TEXT NOT NULL, \
               version INT NOT NULL, \
               event_type TEXT NOT NULL, \
               event_version INT NOT NULL DEFAULT 1, \
               data JSONB NOT NULL, \
               metadata JSONB NOT NULL DEFAULT '{{}}'::jsonb, \
               recorded_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
               CONSTRAINT {EVENTS}_stream_version UNIQUE (aggregate_type, aggregate_id, version))"
        ),
        // The UNIQUE constraint above already indexes (aggregate_type,
        // aggregate_id, version), which is exactly the key `load` reads and orders
        // by — so no extra stream index is needed.
        format!(
            "CREATE TABLE IF NOT EXISTS {SNAPSHOTS} (\
               aggregate_type TEXT NOT NULL, \
               aggregate_id TEXT NOT NULL, \
               version INT NOT NULL, \
               state JSONB NOT NULL, \
               updated_at TIMESTAMPTZ NOT NULL DEFAULT now(), \
               PRIMARY KEY (aggregate_type, aggregate_id))"
        ),
    ]
}

/// An event to append. `event_version` defaults to `1` and is bumped only when the
/// payload schema for this `event_type` changes (see [`Upcasters`]).
#[derive(Debug, Clone)]
pub struct NewEvent {
    pub event_type: String,
    pub event_version: i32,
    pub data: serde_json::Value,
}

impl NewEvent {
    /// A new event of `event_type` carrying `data`, at `event_version` 1.
    pub fn new(event_type: impl Into<String>, data: serde_json::Value) -> Self {
        NewEvent {
            event_type: event_type.into(),
            event_version: 1,
            data,
        }
    }

    /// Override the payload schema version (for writing historical fixtures or a
    /// bumped schema explicitly).
    pub fn at_version(mut self, event_version: i32) -> Self {
        self.event_version = event_version;
        self
    }
}

/// One event as stored, returned by [`load`] in version order.
#[derive(Debug, Clone)]
pub struct StoredEvent {
    /// Total-order position across all aggregates (commit-insertion order).
    pub global_seq: i64,
    pub aggregate_type: String,
    pub aggregate_id: String,
    /// Per-aggregate version, 1-based and gap-free.
    pub version: i32,
    pub event_type: String,
    /// Payload schema version, possibly rewritten up by [`Upcasters`] on load.
    pub event_version: i32,
    pub data: serde_json::Value,
}

/// A cached fold of an aggregate's state at [`version`](Snapshot::version).
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub version: i32,
    pub state: serde_json::Value,
}

/// Append `events` to an aggregate's stream, optimistically.
///
/// `expected_version` is the version the caller believes the stream is at (0 for a
/// brand-new aggregate). The events are written at `expected_version + 1`,
/// `+ 2`, … inside a single transaction ([`db::atomic`]). Returns the new version.
///
/// If the stream is not at `expected_version` — someone else appended in between —
/// the append is refused with [`Error::Conflict`] and nothing is written; the
/// caller re-[`load`]s, re-folds and retries. The in-transaction `MAX(version)`
/// check catches the already-committed case cleanly; a concurrent *uncommitted*
/// appender is invisible to that read and is caught instead by the `UNIQUE
/// (aggregate_type, aggregate_id, version)` constraint (SQLSTATE `23505`), which is
/// mapped back to the same conflict. Appending an empty slice is a no-op that
/// returns `expected_version`.
pub async fn append(
    aggregate_type: &str,
    aggregate_id: &str,
    expected_version: i32,
    events: &[NewEvent],
) -> Result<i32> {
    if events.is_empty() {
        return Ok(expected_version);
    }
    db::atomic(|_tx| async move {
        // The committed current version (0 when the aggregate does not exist yet).
        let rows = db::sql(
            &format!(
                "SELECT COALESCE(MAX(version), 0)::int FROM {EVENTS} \
                 WHERE aggregate_type = $1 AND aggregate_id = $2"
            ),
            &[
                Value::Text(aggregate_type.to_string()),
                Value::Text(aggregate_id.to_string()),
            ],
        )
        .await?;
        let current = rows
            .first()
            .ok_or_else(|| Error::Db("MAX(version) returned no row".into()))?
            .try_get::<i32>(0)?;
        if current != expected_version {
            return Err(Error::Conflict {
                model: aggregate_type.to_string(),
                expected: expected_version as i64,
                actual: Some(current as i64),
            });
        }

        let mut version = expected_version;
        for ev in events {
            version += 1;
            let res = db::run(
                &format!(
                    "INSERT INTO {EVENTS} \
                        (aggregate_type, aggregate_id, version, event_type, event_version, data) \
                     VALUES ($1, $2, $3, $4, $5, $6)"
                ),
                &[
                    Value::Text(aggregate_type.to_string()),
                    Value::Text(aggregate_id.to_string()),
                    Value::I32(version),
                    Value::Text(ev.event_type.clone()),
                    Value::I32(ev.event_version),
                    Value::Json(ev.data.clone()),
                ],
            )
            .await;
            if let Err(e) = res {
                return Err(as_conflict(e, aggregate_type, expected_version));
            }
        }
        Ok(version)
    })
    .await
}

/// Map a unique-violation on the stream's version to a concurrency [`Error::Conflict`].
///
/// A concurrent appender that beat us to this version trips the `UNIQUE
/// (aggregate_type, aggregate_id, version)` constraint (SQLSTATE `23505`). Our
/// in-transaction `MAX(version)` read could not see its uncommitted row, so the
/// constraint is the real backstop — surface it as a conflict, not a bare database
/// error. `actual` is unknown here (the racing writer has not committed), so it is
/// `None`.
fn as_conflict(e: Error, aggregate_type: &str, expected: i32) -> Error {
    if let Error::Db(ref msg) = e {
        if msg.contains("[23505]") {
            tracing::debug!(
                target: "dorm::eventstore",
                aggregate_type,
                expected,
                "append lost the race on the version unique constraint"
            );
            return Error::Conflict {
                model: aggregate_type.to_string(),
                expected: expected as i64,
                actual: None,
            };
        }
    }
    e
}

/// Load an aggregate's full stream in version order.
pub async fn load(aggregate_type: &str, aggregate_id: &str) -> Result<Vec<StoredEvent>> {
    load_since(aggregate_type, aggregate_id, 0).await
}

/// Load the events of an aggregate with `version > after_version`, in version order.
async fn load_since(
    aggregate_type: &str,
    aggregate_id: &str,
    after_version: i32,
) -> Result<Vec<StoredEvent>> {
    let rows = db::sql(
        &format!(
            "SELECT global_seq, version, event_type, event_version, data::text \
             FROM {EVENTS} \
             WHERE aggregate_type = $1 AND aggregate_id = $2 AND version > $3 \
             ORDER BY version"
        ),
        &[
            Value::Text(aggregate_type.to_string()),
            Value::Text(aggregate_id.to_string()),
            Value::I32(after_version),
        ],
    )
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        // JSONB comes back as text and is parsed here, as the outbox/jobs crates
        // do — no JSON column decoder is needed in the core.
        let data_text = r.try_get::<String>(4)?;
        let data = serde_json::from_str(&data_text)
            .map_err(|e| Error::Db(format!("event data is not JSON: {e}")))?;
        out.push(StoredEvent {
            global_seq: r.try_get::<i64>(0)?,
            aggregate_type: aggregate_type.to_string(),
            aggregate_id: aggregate_id.to_string(),
            version: r.try_get::<i32>(1)?,
            event_type: r.try_get::<String>(2)?,
            event_version: r.try_get::<i32>(3)?,
            data,
        });
    }
    Ok(out)
}

/// Save (or advance) the snapshot of an aggregate's folded state at `version`.
///
/// One snapshot per aggregate (the latest). A stale write — an older `version`
/// than the one already stored — is ignored, so an out-of-order snapshotter cannot
/// rewind the cache.
pub async fn save_snapshot(
    aggregate_type: &str,
    aggregate_id: &str,
    version: i32,
    state: &serde_json::Value,
) -> Result<()> {
    db::run(
        &format!(
            "INSERT INTO {SNAPSHOTS} (aggregate_type, aggregate_id, version, state) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (aggregate_type, aggregate_id) DO UPDATE \
               SET version = EXCLUDED.version, state = EXCLUDED.state, updated_at = now() \
             WHERE {SNAPSHOTS}.version <= EXCLUDED.version"
        ),
        &[
            Value::Text(aggregate_type.to_string()),
            Value::Text(aggregate_id.to_string()),
            Value::I32(version),
            Value::Json(state.clone()),
        ],
    )
    .await?;
    Ok(())
}

/// Load an aggregate for a fast rebuild: its latest [`Snapshot`] (if any) and the
/// events recorded **since** that snapshot's version, in order.
///
/// Fold the snapshot's state, then apply the tail events — far cheaper than
/// replaying the whole stream for a long-lived aggregate. With no snapshot, the
/// snapshot is `None` and every event is returned (equivalent to [`load`]).
pub async fn load_from_snapshot(
    aggregate_type: &str,
    aggregate_id: &str,
) -> Result<(Option<Snapshot>, Vec<StoredEvent>)> {
    let rows = db::sql(
        &format!(
            "SELECT version, state::text FROM {SNAPSHOTS} \
             WHERE aggregate_type = $1 AND aggregate_id = $2"
        ),
        &[
            Value::Text(aggregate_type.to_string()),
            Value::Text(aggregate_id.to_string()),
        ],
    )
    .await?;
    let snapshot = match rows.first() {
        Some(r) => {
            let state_text = r.try_get::<String>(1)?;
            let state = serde_json::from_str(&state_text)
                .map_err(|e| Error::Db(format!("snapshot state is not JSON: {e}")))?;
            Some(Snapshot {
                version: r.try_get::<i32>(0)?,
                state,
            })
        }
        None => None,
    };
    let from = snapshot.as_ref().map(|s| s.version).unwrap_or(0);
    let events = load_since(aggregate_type, aggregate_id, from).await?;
    Ok((snapshot, events))
}

/// A payload transform from one `event_version` to the next.
///
/// Given the `data` of an event at version *n*, it returns the `data` as version
/// *n + 1* would have been written.
pub type Upcaster = fn(serde_json::Value) -> serde_json::Value;

/// A registry of [`Upcaster`]s keyed by `(event_type, from_version)`.
///
/// Event payloads on disk are immutable: when an event's schema changes we do not
/// rewrite history, we register an upcaster from the old version to the new and let
/// it run as the event is loaded. Chains compose — a v1→v2 and a v2→v3 upcaster
/// turn a stored v1 event into v3 — so the fold only ever sees the current shape.
#[derive(Default)]
pub struct Upcasters {
    by_key: HashMap<(String, i32), Upcaster>,
}

impl Upcasters {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the transform that takes `event_type` from `from_version` to
    /// `from_version + 1`.
    pub fn register(
        &mut self,
        event_type: impl Into<String>,
        from_version: i32,
        f: Upcaster,
    ) -> &mut Self {
        self.by_key.insert((event_type.into(), from_version), f);
        self
    }

    /// Apply the registered chain to one event, lifting its payload (and
    /// `event_version`) to the highest version reachable.
    pub fn upcast(&self, mut ev: StoredEvent) -> StoredEvent {
        while let Some(f) = self.by_key.get(&(ev.event_type.clone(), ev.event_version)) {
            ev.data = f(ev.data);
            ev.event_version += 1;
        }
        ev
    }

    /// [`load`] the aggregate's stream and upcast every event in it.
    pub async fn load(&self, aggregate_type: &str, aggregate_id: &str) -> Result<Vec<StoredEvent>> {
        let raw = load(aggregate_type, aggregate_id).await?;
        Ok(raw.into_iter().map(|e| self.upcast(e)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_migrations_create_both_tables_with_the_stream_uniqueness() {
        let ddl = migrations().join("\n");
        assert!(ddl.contains(EVENTS));
        assert!(ddl.contains(SNAPSHOTS));
        assert!(ddl.contains("global_seq BIGSERIAL"));
        // The optimistic-concurrency guard: no two events at the same version.
        assert!(ddl.contains(&format!("{EVENTS}_stream_version")));
        assert!(ddl.contains("UNIQUE (aggregate_type, aggregate_id, version)"));
        // One snapshot per aggregate.
        assert!(ddl.contains("PRIMARY KEY (aggregate_type, aggregate_id)"));
    }

    #[test]
    fn new_event_defaults_to_event_version_one() {
        let e = NewEvent::new("Created", serde_json::json!({"x": 1}));
        assert_eq!(e.event_version, 1);
        assert_eq!(e.event_type, "Created");
        assert_eq!(e.at_version(3).event_version, 3);
    }

    #[test]
    fn upcasters_chain_v1_to_v3_and_stop() {
        let mut up = Upcasters::new();
        // v1→v2: split `name` into `first`/`last`.
        up.register("Registered", 1, |mut d| {
            let name = d["name"].as_str().unwrap_or("").to_string();
            let (first, last) = name.split_once(' ').unwrap_or((name.as_str(), ""));
            d["first"] = serde_json::json!(first);
            d["last"] = serde_json::json!(last);
            d
        });
        // v2→v3: add a default `active` flag.
        up.register("Registered", 2, |mut d| {
            d["active"] = serde_json::json!(true);
            d
        });

        let stored = StoredEvent {
            global_seq: 1,
            aggregate_type: "User".into(),
            aggregate_id: "u1".into(),
            version: 1,
            event_type: "Registered".into(),
            event_version: 1,
            data: serde_json::json!({"name": "Ada Lovelace"}),
        };
        let up_ev = up.upcast(stored);
        assert_eq!(up_ev.event_version, 3, "chained v1→v2→v3");
        assert_eq!(up_ev.data["first"], serde_json::json!("Ada"));
        assert_eq!(up_ev.data["last"], serde_json::json!("Lovelace"));
        assert_eq!(up_ev.data["active"], serde_json::json!(true));
    }

    #[test]
    fn upcasting_an_already_current_event_is_a_no_op() {
        let mut up = Upcasters::new();
        up.register("E", 1, |mut d| {
            d["v"] = serde_json::json!(2);
            d
        });
        // No upcaster registered at version 2, so a v2 event is returned untouched.
        let stored = StoredEvent {
            global_seq: 9,
            aggregate_type: "A".into(),
            aggregate_id: "a".into(),
            version: 5,
            event_type: "E".into(),
            event_version: 2,
            data: serde_json::json!({"v": 2}),
        };
        let out = up.upcast(stored);
        assert_eq!(out.event_version, 2);
        assert_eq!(out.data, serde_json::json!({"v": 2}));
    }

    #[test]
    fn as_conflict_maps_only_a_unique_violation() {
        let conflict = as_conflict(Error::Db("ERROR [23505]: duplicate key".into()), "Order", 4);
        assert!(matches!(
            conflict,
            Error::Conflict {
                expected: 4,
                actual: None,
                ..
            }
        ));
        // An unrelated database error passes through unchanged.
        let other = as_conflict(Error::Db("ERROR [40001]: serialization".into()), "Order", 4);
        assert!(matches!(other, Error::Db(_)));
    }
}
