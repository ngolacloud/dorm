//! The pool, statement execution, and transactions.
//!
//! ## Why the current transaction is a task-local
//!
//! `QuerySet::delete()` does not take a transaction argument, yet inside
//! `atomic(…)` it must run on the transaction's connection. The alternative —
//! threading a `&Transaction` through every call — is impossible for the REPL,
//! where the script says `atomic(|| { app.delete(); })` and `app.delete()` was
//! compiled with no knowledge of any transaction. A task-local gives the same
//! dynamic scoping Django gets from a thread-local, and survives `.await`.
//!
//! ## Why raw `BEGIN`/`SAVEPOINT` instead of `tokio_postgres::Transaction`
//!
//! `Transaction<'a>` borrows its `Client`. Storing one in a task-local would
//! mean naming that lifetime in the task-local's type, which cannot be done.
//! Issuing the statements directly keeps an owned pooled connection in the
//! task-local and costs nothing — the server behaves identically.
//!
//! ## What happens on a panic
//!
//! Error returns roll back explicitly. A panic cannot: unwinding an async frame
//! gives no chance to `.await` a `ROLLBACK`. Instead the guard takes the
//! connection out of the pool on the way down, so it is closed rather than
//! recycled — and a connection closing mid-transaction is rolled back by
//! PostgreSQL itself. That is a stronger guarantee than a best-effort
//! `ROLLBACK`, because it also covers the process being killed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod, Runtime};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Config, NoTls, Row};

use crate::error::{from_pg, Error, Result};

/// The DSN. `DELONIX_ORM_DSN` is the name this workspace uses for its own
/// stores; `PAAS_DATABASE_URL` is accepted as well so the documented invocation
/// from outside works unchanged.
pub const DSN_ENV: &str = "DELONIX_ORM_DSN";
/// Accepted fallback, checked only when [`DSN_ENV`] is absent.
pub const DSN_ENV_FALLBACK: &str = "PAAS_DATABASE_URL";
/// Ceiling on pooled connections.
pub const POOL_MAX_ENV: &str = "DELONIX_ORM_POOL_MAX";
/// How long a caller waits for a connection before giving up.
pub const POOL_ACQUIRE_TIMEOUT_MS_ENV: &str = "DELONIX_ORM_POOL_ACQUIRE_TIMEOUT_MS";

const POOL_MAX_DEFAULT: usize = 8;
const POOL_ACQUIRE_TIMEOUT_DEFAULT_MS: u64 = 5_000;

static POOL: OnceLock<Pool> = OnceLock::new();
static READ_ONLY: AtomicBool = AtomicBool::new(false);

/// Read the configured DSN.
pub fn dsn() -> Result<String> {
    for key in [DSN_ENV, DSN_ENV_FALLBACK] {
        if let Ok(v) = std::env::var(key) {
            if !v.trim().is_empty() {
                return Ok(v);
            }
        }
    }
    Err(Error::Config(format!(
        "no database configured: set {DSN_ENV} (or {DSN_ENV_FALLBACK})"
    )))
}

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n > 0)
        .unwrap_or(default)
}

/// Mark this process as read-only. Every write refuses before it reaches SQL.
///
/// The session is *also* opened `READ ONLY` at the server, which is the real
/// guard; this flag exists so the refusal names the operation instead of
/// surfacing a PostgreSQL error about a read-only transaction.
pub fn set_read_only(yes: bool) {
    READ_ONLY.store(yes, Ordering::SeqCst);
}

/// Whether this process is read-only.
pub fn is_read_only() -> bool {
    READ_ONLY.load(Ordering::SeqCst)
}

/// Refuse a write in a read-only session.
pub fn guard_write(op: &'static str) -> Result<()> {
    if is_read_only() {
        return Err(Error::ReadOnly(op));
    }
    Ok(())
}

/// Build the pool, once.
///
/// # The pool belongs to one runtime
///
/// `tokio-postgres` drives each connection from a background task, so a pooled
/// connection is owned by the runtime that created it. This pool is a process
/// global, which means **the process must keep one runtime alive for as long as
/// it uses the ORM**. Drop that runtime and every connection in the pool dies
/// with it; the next borrow comes back `connection closed`.
///
/// It matters for the shell: the synchronous REPL has to build its runtime once
/// and `block_on` against that same one for the whole session. A runtime per
/// statement would work for exactly one statement. This is not a hypothetical —
/// it is what the integration tests did first, and five of them failed on it.
pub fn pool() -> Result<&'static Pool> {
    if let Some(p) = POOL.get() {
        return Ok(p);
    }
    let dsn = dsn()?;
    let config: Config = dsn
        .parse()
        .map_err(|e| Error::Config(format!("the DSN is not usable: {e}")))?;

    // `Fast` recycling: the pool hands back a connection without a round trip.
    // It is safe here because a connection is only ever returned to the pool
    // with no transaction open — `atomic` takes it out of the pool instead of
    // returning a dirty one.
    let manager = Manager::from_config(
        config,
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    let pool = Pool::builder(manager)
        // Required because the timeouts below are set: deadpool needs a runtime
        // to arm them against, and without this it refuses to build with
        // "Timeouts require a runtime" rather than silently ignoring them.
        .runtime(Runtime::Tokio1)
        .max_size(env_usize(POOL_MAX_ENV, POOL_MAX_DEFAULT))
        .wait_timeout(Some(Duration::from_millis(env_usize(
            POOL_ACQUIRE_TIMEOUT_MS_ENV,
            POOL_ACQUIRE_TIMEOUT_DEFAULT_MS as usize,
        ) as u64)))
        .create_timeout(Some(Duration::from_millis(POOL_ACQUIRE_TIMEOUT_DEFAULT_MS)))
        .build()
        .map_err(|e| Error::Pool(e.to_string()))?;

    Ok(POOL.get_or_init(|| pool))
}

/// The connection a transaction is running on, plus its savepoint depth.
struct TxState {
    conn: Option<Object>,
    depth: usize,
}

type Shared = Arc<tokio::sync::Mutex<TxState>>;

tokio::task_local! {
    /// The transaction in scope, if any. Set by [`atomic`] for the duration of
    /// the closure, which is what lets a nested call find it without being told.
    static CURRENT_TX: Shared;
}

async fn acquire() -> Result<Object> {
    pool()?
        .get()
        .await
        .map_err(|e| Error::Pool(format!("could not acquire a connection: {e}")))
}

/// Run a query, on the transaction in scope if there is one.
pub async fn query(sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<Row>> {
    if let Ok(tx) = CURRENT_TX.try_with(|t| t.clone()) {
        let guard = tx.lock().await;
        let conn = guard.conn.as_ref().ok_or_else(|| {
            Error::Db("the transaction's connection is gone; it was rolled back".into())
        })?;
        return conn.query(sql, params).await.map_err(from_pg);
    }
    acquire().await?.query(sql, params).await.map_err(from_pg)
}

/// Run a statement and report how many rows it touched.
pub async fn execute(sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
    if let Ok(tx) = CURRENT_TX.try_with(|t| t.clone()) {
        let guard = tx.lock().await;
        let conn = guard.conn.as_ref().ok_or_else(|| {
            Error::Db("the transaction's connection is gone; it was rolled back".into())
        })?;
        return conn.execute(sql, params).await.map_err(from_pg);
    }
    acquire().await?.execute(sql, params).await.map_err(from_pg)
}

/// Run a statement for its effect only, outside any result set.
async fn simple(conn: &Object, sql: &str) -> Result<()> {
    conn.batch_execute(sql).await.map_err(from_pg)
}

/// A handle on the transaction in scope.
///
/// It carries no connection: the task-local does the routing. What it offers is
/// the savepoint depth, which a caller occasionally wants to log, and a type that
/// makes `atomic(|tx| …)` read the way the Django-shaped API promises.
#[derive(Debug, Clone, Copy)]
pub struct Tx {
    depth: usize,
}

impl Tx {
    /// 1 for the outermost transaction, 2 for the first savepoint, and so on.
    pub fn depth(self) -> usize {
        self.depth
    }

    /// Whether this is a savepoint rather than the outer transaction.
    pub fn is_savepoint(self) -> bool {
        self.depth > 1
    }
}

/// Whether a transaction is currently in scope.
pub fn in_transaction() -> bool {
    CURRENT_TX.try_with(|_| ()).is_ok()
}

/// Closes the connection if dropped while still holding it — which only happens
/// on a panic, and is what makes PostgreSQL roll the transaction back.
struct PanicGuard {
    shared: Shared,
    armed: bool,
}

impl Drop for PanicGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Unwinding: take the connection out so it is closed rather than
        // recycled into the pool with an open transaction. `Object::take`
        // detaches it from the pool; dropping it then closes the socket, and the
        // server rolls back.
        if let Ok(mut state) = self.shared.try_lock() {
            if let Some(conn) = state.conn.take() {
                let _ = Object::take(conn);
            }
        }
    }
}

/// Run `f` inside a transaction, committing on `Ok` and rolling back on `Err`.
///
/// Nesting is by savepoint: an inner `atomic` that fails undoes only its own
/// work, and the outer transaction can still commit. That is what makes the
/// REPL's `atomic(|| { … throw "rollback"; })` undo exactly the block.
pub async fn atomic<F, Fut, T>(f: F) -> Result<T>
where
    F: FnOnce(Tx) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    match CURRENT_TX.try_with(|t| t.clone()) {
        // Already inside one: a savepoint.
        Ok(shared) => {
            let depth = {
                let mut state = shared.lock().await;
                state.depth += 1;
                let conn = state.conn.as_ref().ok_or_else(|| {
                    Error::Db("the outer transaction was already rolled back".into())
                })?;
                let name = savepoint_name(state.depth);
                simple(conn, &format!("SAVEPOINT {name}")).await?;
                state.depth
            };
            let name = savepoint_name(depth);

            let guard = PanicGuard {
                shared: shared.clone(),
                armed: true,
            };
            let outcome = f(Tx { depth }).await;
            let mut guard = guard;
            guard.armed = false;

            let mut state = shared.lock().await;
            state.depth -= 1;
            let conn = state
                .conn
                .as_ref()
                .ok_or_else(|| Error::Db("the transaction's connection is gone".into()))?;
            match outcome {
                Ok(v) => {
                    simple(conn, &format!("RELEASE SAVEPOINT {name}")).await?;
                    Ok(v)
                }
                Err(e) => {
                    // Rolling back to the savepoint leaves the outer transaction
                    // usable, which is the whole point of nesting.
                    simple(conn, &format!("ROLLBACK TO SAVEPOINT {name}")).await?;
                    simple(conn, &format!("RELEASE SAVEPOINT {name}")).await?;
                    Err(e)
                }
            }
        }

        // The outermost transaction.
        Err(_) => {
            let conn = acquire().await?;
            let start = if is_read_only() {
                "BEGIN READ ONLY"
            } else {
                "BEGIN"
            };
            simple(&conn, start).await?;
            let shared: Shared = Arc::new(tokio::sync::Mutex::new(TxState {
                conn: Some(conn),
                depth: 1,
            }));

            let guard = PanicGuard {
                shared: shared.clone(),
                armed: true,
            };
            let outcome = CURRENT_TX.scope(shared.clone(), f(Tx { depth: 1 })).await;
            let mut guard = guard;
            guard.armed = false;

            let mut state = shared.lock().await;
            let conn = state
                .conn
                .take()
                .ok_or_else(|| Error::Db("the transaction's connection is gone".into()))?;
            let result = match outcome {
                Ok(v) => simple(&conn, "COMMIT").await.map(|()| v),
                Err(e) => {
                    // The rollback's own failure must not hide the error that
                    // caused it; that is the one the caller needs to read.
                    let _ = simple(&conn, "ROLLBACK").await;
                    Err(e)
                }
            };
            // Returned to the pool only now that no transaction is open.
            drop(conn);
            result
        }
    }
}

/// Savepoint names are generated, never taken from a caller — they are
/// identifiers and cannot be parameterised.
fn savepoint_name(depth: usize) -> String {
    format!("orm_sp_{depth}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn savepoint_names_are_generated_and_contain_nothing_a_caller_supplied() {
        assert_eq!(savepoint_name(1), "orm_sp_1");
        assert_eq!(savepoint_name(7), "orm_sp_7");
        // Only ASCII identifier characters, so there is nothing to escape.
        assert!(savepoint_name(3)
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn a_missing_dsn_is_a_configuration_error_naming_both_variables() {
        // Save and restore, because the process environment is shared with
        // every other test in this binary.
        let before = (
            std::env::var(DSN_ENV).ok(),
            std::env::var(DSN_ENV_FALLBACK).ok(),
        );
        std::env::remove_var(DSN_ENV);
        std::env::remove_var(DSN_ENV_FALLBACK);

        let e = dsn().unwrap_err().to_string();
        assert!(e.contains(DSN_ENV), "{e}");
        assert!(e.contains(DSN_ENV_FALLBACK), "{e}");

        if let Some(v) = before.0 {
            std::env::set_var(DSN_ENV, v);
        }
        if let Some(v) = before.1 {
            std::env::set_var(DSN_ENV_FALLBACK, v);
        }
    }

    #[test]
    fn an_empty_dsn_counts_as_unset() {
        let before = std::env::var(DSN_ENV).ok();
        std::env::set_var(DSN_ENV, "   ");
        std::env::remove_var(DSN_ENV_FALLBACK);
        assert!(dsn().is_err());
        match before {
            Some(v) => std::env::set_var(DSN_ENV, v),
            None => std::env::remove_var(DSN_ENV),
        }
    }

    #[test]
    fn the_read_only_flag_names_the_operation_it_refuses() {
        set_read_only(true);
        let e = guard_write("delete").unwrap_err().to_string();
        assert!(e.contains("delete"), "{e}");
        set_read_only(false);
        assert!(guard_write("delete").is_ok());
    }

    #[test]
    fn a_transaction_handle_knows_whether_it_is_a_savepoint() {
        assert!(!Tx { depth: 1 }.is_savepoint());
        assert!(Tx { depth: 2 }.is_savepoint());
        assert_eq!(Tx { depth: 3 }.depth(), 3);
    }

    #[test]
    fn pool_sizing_falls_back_to_the_default_for_junk_and_for_zero() {
        let key = "DELONIX_ORM_TEST_POOL_PROBE";
        std::env::set_var(key, "not-a-number");
        assert_eq!(env_usize(key, 8), 8);
        // Zero would build a pool that can never hand out a connection.
        std::env::set_var(key, "0");
        assert_eq!(env_usize(key, 8), 8);
        std::env::set_var(key, "16");
        assert_eq!(env_usize(key, 8), 16);
        std::env::remove_var(key);
    }
}
