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

use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use deadpool_postgres::{Manager, ManagerConfig, Object, Pool, RecyclingMethod, Runtime};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Config, NoTls, Row as PgRow};

use crate::compiler::Dialect;
use crate::error::{from_pg, Error, Result};
use crate::row::Row;
use crate::value::Value;

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
/// Statements slower than this many milliseconds are logged at `warn` (ADR 0008).
/// Absent — or zero — disables the slow-query log entirely, which is the default:
/// production opts *in* to the noise, it is never surprised by it.
pub const SLOW_QUERY_MS_ENV: &str = "DELONIX_ORM_SLOW_QUERY_MS";

const POOL_MAX_DEFAULT: usize = 8;
const POOL_ACQUIRE_TIMEOUT_DEFAULT_MS: u64 = 5_000;

/// How much SQL a log line carries before it is truncated. Long enough to read
/// the shape of a statement, short enough that a 50-line generated query does
/// not flood the log. The full SQL is never a secret — the *parameters* are, and
/// those are never here (see [`observe`]).
const SQL_LOG_MAX: usize = 300;

static POOL: OnceLock<Pool> = OnceLock::new();
static READ_ONLY: AtomicBool = AtomicBool::new(false);

/// The slow-query threshold, read from the environment exactly once.
///
/// Like the pool sizing, this is process configuration set at launch, not a
/// per-call knob — so it is resolved on first use and cached, keeping the hot
/// path free of an env lookup (and its lock + allocation) on every statement.
/// `None` means the slow-query log is off; `Some(d)` is the threshold.
static SLOW_QUERY: OnceLock<Option<Duration>> = OnceLock::new();

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

// ------------------------------------------------------- observability (ADR 0008)
//
// Every statement that runs through `query`/`execute` emits one `tracing` event
// with its timing and shape, and — when a threshold is configured — a `warn`
// when it runs long. The whole thing is a no-op without a subscriber, which is
// why it is always compiled in rather than feature-gated: `tracing`'s disabled
// path is a relaxed atomic load, so a process that installs no subscriber pays
// essentially nothing. See ADR 0008.
//
// SECURITY — this is the invariant the whole module keeps: a log line carries
// the SQL *text* (which holds `$n` placeholders, never literals) and metadata.
// It NEVER carries the bound parameter values, because those are exactly the
// data most likely to be sensitive — a password being hashed, an email, a token.
// `observe` is given the SQL and a row count, never `params`, so there is no path
// by which a value can reach the log.

/// The configured slow-query threshold, resolved once. See [`SLOW_QUERY`].
fn slow_query_threshold() -> Option<Duration> {
    *SLOW_QUERY.get_or_init(|| {
        std::env::var(SLOW_QUERY_MS_ENV)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
    })
}

/// Shorten SQL for a log line, on a UTF-8 boundary, tagging the original length
/// when it is cut. Only the SQL text is ever summarised — never a parameter
/// value, which does not pass through here (see the module note above).
fn summarize_sql(sql: &str) -> Cow<'_, str> {
    let trimmed = sql.trim();
    if trimmed.len() <= SQL_LOG_MAX {
        return Cow::Borrowed(trimmed);
    }
    let mut end = SQL_LOG_MAX;
    while !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}… [{} bytes]", &trimmed[..end], trimmed.len()))
}

/// Emit the per-statement trace event and, past the configured threshold, the
/// slow-query warning.
///
/// `rows` is `Some(n)` when the statement succeeded — rows returned for a query,
/// rows affected for an execute — and `None` when it failed; the failure itself
/// still propagates to the caller as an `Err`, this only records that it ran.
/// `params` is deliberately not a parameter of this function: values never log.
fn observe(op: &'static str, sql: &str, elapsed: Duration, rows: Option<u64>, in_tx: bool) {
    let duration_ms = elapsed.as_secs_f64() * 1e3;
    let sql = summarize_sql(sql);

    // The normal case, at debug: one line per statement for anyone who turns the
    // `dorm::db` target up, invisible (and nearly free) to everyone else.
    tracing::debug!(
        target: "dorm::db",
        op,
        rows = ?rows,
        duration_ms,
        in_tx,
        sql = %sql,
        "statement finished"
    );

    if let Some(threshold) = slow_query_threshold() {
        if elapsed >= threshold {
            // A separate `warn` so slow queries surface on a default subscriber
            // without having to opt into the per-statement debug stream.
            tracing::warn!(
                target: "dorm::db",
                op,
                duration_ms,
                threshold_ms = threshold.as_millis() as u64,
                in_tx,
                sql = %sql,
                "slow query"
            );
        }
    }
}

/// A point-in-time snapshot of the connection pool's occupancy (ADR 0008).
///
/// This mirrors deadpool's own `Status` but is a type DORM owns, so the pool
/// implementation does not leak into the public API — a caller can serialize it
/// into a metrics endpoint or a health check without taking a deadpool
/// dependency, and we can change pools underneath without breaking them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PoolStats {
    /// The configured ceiling — `DELONIX_ORM_POOL_MAX`, or the built-in default.
    pub max_size: usize,
    /// Connections currently managed by the pool (idle plus checked-out).
    pub size: usize,
    /// Idle connections available to hand out right now, as deadpool reports it.
    pub available: usize,
    /// Callers currently blocked waiting for a connection.
    pub waiting: usize,
}

/// A snapshot of the pool's current occupancy, for a metrics endpoint or health
/// check (ADR 0008). Errors only if no pool can be built — i.e. no DSN is set.
///
/// It is a snapshot, not a subscription: the numbers are already stale by the
/// time the caller reads them, which is all a pool gauge ever is. Cheap enough
/// to call per scrape.
pub fn pool_stats() -> Result<PoolStats> {
    let status = pool()?.status();
    Ok(PoolStats {
        max_size: status.max_size,
        size: status.size,
        available: status.available,
        waiting: status.waiting,
    })
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
///
/// This is the PostgreSQL-native entry point: it returns `tokio_postgres` rows
/// and the raw callers (the CLI, the migrator, the shell) consume them with the
/// driver's own API. The typed `QuerySet` goes through the backend-agnostic
/// [`fetch`] instead, which wraps these rows in a [`Row`].
///
/// ## Why `prepare_cached` and not `conn.query(sql, …)`
///
/// `Client::query(&str, …)` prepares an unnamed statement on every call — a
/// Parse round trip the server throws away each time. `prepare_cached` keeps a
/// named statement per connection (deadpool's [`StatementCache`], ADR 0007), so
/// the same SQL — and the ORM emits the same text with `$n` placeholders for a
/// given query — pays Parse once and is Bind/Execute-only thereafter. The win
/// grows with repetition, which is the ORM's whole access pattern.
pub async fn query(sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<PgRow>> {
    // Observed end to end (ADR 0008): the clock spans Parse-or-cache-hit plus
    // Bind/Execute, which is the latency the caller actually pays. `in_tx` is
    // read up front so the event records the routing even on the error path.
    let in_tx = in_transaction();
    let start = Instant::now();
    let result = query_inner(sql, params).await;
    observe(
        "query",
        sql,
        start.elapsed(),
        result.as_ref().ok().map(|rows| rows.len() as u64),
        in_tx,
    );
    result
}

async fn query_inner(sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<PgRow>> {
    if let Ok(tx) = CURRENT_TX.try_with(|t| t.clone()) {
        let guard = tx.lock().await;
        let conn = guard.conn.as_ref().ok_or_else(|| {
            Error::Db("the transaction's connection is gone; it was rolled back".into())
        })?;
        let stmt = conn.prepare_cached(sql).await.map_err(from_pg)?;
        return conn.query(&stmt, params).await.map_err(from_pg);
    }
    let conn = acquire().await?;
    let stmt = conn.prepare_cached(sql).await.map_err(from_pg)?;
    conn.query(&stmt, params).await.map_err(from_pg)
}

/// Run a statement and report how many rows it touched.
///
/// Statements are cached per connection exactly as in [`query`]; see its note
/// on `prepare_cached` (ADR 0007).
pub async fn execute(sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
    // Observed exactly as `query` is (ADR 0008); here the recorded row count is
    // rows *affected* rather than returned.
    let in_tx = in_transaction();
    let start = Instant::now();
    let result = execute_inner(sql, params).await;
    observe(
        "execute",
        sql,
        start.elapsed(),
        result.as_ref().ok().copied(),
        in_tx,
    );
    result
}

async fn execute_inner(sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<u64> {
    if let Ok(tx) = CURRENT_TX.try_with(|t| t.clone()) {
        let guard = tx.lock().await;
        let conn = guard.conn.as_ref().ok_or_else(|| {
            Error::Db("the transaction's connection is gone; it was rolled back".into())
        })?;
        let stmt = conn.prepare_cached(sql).await.map_err(from_pg)?;
        return conn.execute(&stmt, params).await.map_err(from_pg);
    }
    let conn = acquire().await?;
    let stmt = conn.prepare_cached(sql).await.map_err(from_pg)?;
    conn.execute(&stmt, params).await.map_err(from_pg)
}

/// Run a statement for its effect only, outside any result set.
async fn simple(conn: &Object, sql: &str) -> Result<()> {
    conn.batch_execute(sql).await.map_err(from_pg)
}

// ------------------------------------------------------------- backend seam
//
// ADR 0003: the typed `QuerySet` terminals resolve their backend from the DSN
// and dispatch here. PostgreSQL reuses the pool above; SQLite runs `rusqlite`
// on `spawn_blocking` (it is synchronous) over a single shared connection, so
// that `sqlite::memory:` is one database rather than one per connection. A
// connection pool can come later; correctness of the in-memory case comes
// first, and SQLite serialises writers regardless.

/// The backend a DSN names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Postgres,
    Sqlite,
}

/// Which backend the configured DSN selects.
///
/// `postgres(ql)://…` and the libpq keyword form (`host=… user=…`) are
/// PostgreSQL; `sqlite:…`, `file:…` and a bare `:memory:` are SQLite. PostgreSQL
/// is the default for anything unrecognised, which keeps the historical
/// behaviour for a DSN that names no scheme.
pub fn backend_of(dsn: &str) -> Backend {
    let d = dsn.trim();
    let lower = d.to_ascii_lowercase();
    if lower.starts_with("sqlite:") || lower.starts_with("file:") || d == ":memory:" {
        Backend::Sqlite
    } else {
        Backend::Postgres
    }
}

/// The configured backend, or an error if no DSN is set.
fn backend() -> Result<Backend> {
    Ok(backend_of(&dsn()?))
}

/// The SQL dialect the configured backend speaks.
///
/// Used by the `QuerySet` terminals to compile with `compile_*_on`. It never
/// fails: with no DSN set it answers PostgreSQL, so offline compilation of a
/// query's SQL still works — the missing DSN surfaces later, when the query is
/// actually run.
pub fn dialect() -> Dialect {
    match dsn() {
        Ok(d) if backend_of(&d) == Backend::Sqlite => Dialect::Sqlite,
        _ => Dialect::Postgres,
    }
}

/// Run a row-returning statement against the configured backend, taking ORM
/// values and handing back backend-agnostic [`Row`]s.
///
/// Both paths keep the transaction-in-scope routing: PostgreSQL reuses [`query`]
/// (routed by `CURRENT_TX`); SQLite checks its own transaction task-local and,
/// inside an [`atomic`], runs on the connection the transaction already holds.
pub async fn fetch(sql: &str, params: &[Value]) -> Result<Vec<Row>> {
    match backend()? {
        Backend::Postgres => {
            let borrowed: Vec<&(dyn ToSql + Sync)> =
                params.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
            Ok(query(sql, &borrowed)
                .await?
                .into_iter()
                .map(Row::Pg)
                .collect())
        }
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => sqlite::fetch(sql, params).await,
        #[cfg(not(feature = "sqlite"))]
        Backend::Sqlite => Err(sqlite_disabled()),
    }
}

/// Run a statement for its row count against the configured backend.
pub async fn run(sql: &str, params: &[Value]) -> Result<u64> {
    match backend()? {
        Backend::Postgres => {
            let borrowed: Vec<&(dyn ToSql + Sync)> =
                params.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
            execute(sql, &borrowed).await
        }
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => sqlite::run(sql, params).await,
        #[cfg(not(feature = "sqlite"))]
        Backend::Sqlite => Err(sqlite_disabled()),
    }
}

// ------------------------------------------------------- raw SQL (ADR 0005)
//
// The sanctioned escape hatch for a query the compiler cannot build. Every
// value is still a bound parameter; the text between the placeholders is the
// caller's. These route through the same backend seam as the typed path, so a
// raw statement observes the transaction in scope, the pool, the statement
// timeout, TLS and the read-only guard — nothing steps outside those.

/// Raw read: run hand-written SQL and get untyped [`Row`]s back.
///
/// For a result that maps onto a model, use `Manager::raw` instead, which runs
/// this and decodes each row with `from_row`. Every `params` element is bound as
/// `$n`/`?n`; the SQL text is the caller's (ADR 0005).
pub async fn sql(sql: &str, params: &[Value]) -> Result<Vec<Row>> {
    fetch(sql, params).await
}

/// Raw write: run hand-written DML and report the rows affected.
///
/// Refused under [`set_read_only`] like every other write, with a named
/// [`Error::ReadOnly`] — so a read-only session stays read-only even through the
/// escape hatch (ADR 0005).
pub async fn sql_execute(sql: &str, params: &[Value]) -> Result<u64> {
    guard_write("sql_execute")?;
    run(sql, params).await
}

/// Run DDL — one or more parameterless statements — against the configured
/// backend. Used to stand a schema up before the typed path queries it.
pub async fn run_ddl(ddl: &str) -> Result<()> {
    match backend()? {
        Backend::Postgres => simple(&acquire().await?, ddl).await,
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => sqlite::run_ddl(ddl).await,
        #[cfg(not(feature = "sqlite"))]
        Backend::Sqlite => Err(sqlite_disabled()),
    }
}

/// The error a SQLite DSN gets in a build that left the feature off.
#[cfg(not(feature = "sqlite"))]
fn sqlite_disabled() -> Error {
    Error::Config(
        "the DSN selects SQLite, but this build has no SQLite backend; \
         rebuild dorm-core with features = [\"sqlite\"]"
            .into(),
    )
}

/// The SQLite backend: a single shared connection, driven synchronously under
/// `spawn_blocking`.
///
/// ## Why transactions need a lock the whole connection holds for their scope
///
/// PostgreSQL gets isolation for free: `atomic` takes a *dedicated* pooled
/// connection, and every other task works on a different one. SQLite has one
/// connection for the whole process, so the only way another task cannot observe
/// the connection mid-`BEGIN` is to deny it the connection for the transaction's
/// scope. That is [`TX_LOCK`]: an async mutex the outer `atomic` holds from
/// `BEGIN` to `COMMIT`/`ROLLBACK`, and that every standalone statement acquires
/// per call. The per-statement `Mutex` on the connection itself is still taken
/// inside each `spawn_blocking` — but it is released between statements, so it
/// cannot be what keeps a transaction private; `TX_LOCK` is.
///
/// Inside the transaction, the closure's own `fetch`/`run` must *not* re-acquire
/// `TX_LOCK` — the transaction already holds it — so a [`SQLITE_TX`] task-local
/// marks the scope and those calls run straight on the connection.
#[cfg(feature = "sqlite")]
mod sqlite {
    use std::sync::{Arc, Mutex, OnceLock};

    use super::{dsn, savepoint_name, Error, Result, Tx};
    use crate::row::{value_to_sqlite, Row, SqliteRow};
    use crate::value::Value;

    static CONN: OnceLock<Arc<Mutex<rusqlite::Connection>>> = OnceLock::new();

    /// Serialises logical access to the single connection. An outer `atomic`
    /// holds it for its whole scope; a standalone statement holds it for one call.
    /// This — not the per-call connection `Mutex` — is what keeps a transaction
    /// from being observed mid-flight by another task.
    static TX_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();

    /// The savepoint depth of the transaction in scope, shared so a nested
    /// `atomic` (a savepoint) can bump it and the outer frame can read it back.
    #[derive(Clone)]
    struct SqliteTxState {
        depth: Arc<Mutex<usize>>,
    }

    tokio::task_local! {
        /// Set by the outer [`atomic`] for the duration of its closure. Its
        /// presence is what tells a standalone statement it is already inside a
        /// transaction and must not re-acquire [`TX_LOCK`].
        static SQLITE_TX: SqliteTxState;
    }

    fn tx_lock() -> Arc<tokio::sync::Mutex<()>> {
        TX_LOCK
            .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Whether a SQLite transaction is in scope on this task.
    fn in_tx() -> bool {
        SQLITE_TX.try_with(|_| ()).is_ok()
    }

    /// Acquire the connection lock for a standalone statement, or `None` when a
    /// transaction already holds it. Held across the statement's `.await`.
    async fn lock_if_standalone() -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if in_tx() {
            None
        } else {
            Some(tx_lock().lock_owned().await)
        }
    }

    /// The shared connection, opened once from the DSN.
    fn conn() -> Result<Arc<Mutex<rusqlite::Connection>>> {
        if let Some(c) = CONN.get() {
            return Ok(c.clone());
        }
        let dsn = dsn()?;
        let c = open(&dsn)?;
        Ok(CONN.get_or_init(|| Arc::new(Mutex::new(c))).clone())
    }

    /// Open a connection for the DSN. `sqlite::memory:`, `sqlite:` and `:memory:`
    /// are in-memory; anything else is the file path after the scheme.
    fn open(dsn: &str) -> Result<rusqlite::Connection> {
        let rest = dsn
            .trim()
            .strip_prefix("sqlite:")
            .or_else(|| dsn.trim().strip_prefix("file:"))
            .unwrap_or_else(|| dsn.trim());
        let conn = if rest.is_empty() || rest == ":memory:" {
            rusqlite::Connection::open_in_memory()
        } else {
            rusqlite::Connection::open(rest)
        };
        conn.map_err(|e| Error::Db(e.to_string()))
    }

    pub(super) async fn fetch(sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let _lock = lock_if_standalone().await;
        fetch_on_conn(sql, params).await
    }

    pub(super) async fn run(sql: &str, params: &[Value]) -> Result<u64> {
        let _lock = lock_if_standalone().await;
        run_on_conn(sql, params).await
    }

    pub(super) async fn run_ddl(ddl: &str) -> Result<()> {
        let _lock = lock_if_standalone().await;
        let conn = conn()?;
        let ddl = ddl.to_string();
        blocking(move || {
            let guard = conn.lock().map_err(|_| poisoned())?;
            guard.execute_batch(&ddl).map_err(db_err)
        })
        .await
    }

    /// Run a row-returning statement on the connection, assuming the caller holds
    /// the connection lock (standalone statement) or is inside a transaction.
    async fn fetch_on_conn(sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let conn = conn()?;
        let sql = sql.to_string();
        let params: Vec<rusqlite::types::Value> = params.iter().map(value_to_sqlite).collect();
        blocking(move || {
            let guard = conn.lock().map_err(|_| poisoned())?;
            let mut stmt = guard.prepare(&sql).map_err(db_err)?;
            let ncols = stmt.column_count();
            let mut rows = stmt
                .query(rusqlite::params_from_iter(params))
                .map_err(db_err)?;
            let mut out = Vec::new();
            while let Some(r) = rows.next().map_err(db_err)? {
                let mut cells = Vec::with_capacity(ncols);
                for i in 0..ncols {
                    cells.push(r.get::<usize, rusqlite::types::Value>(i).map_err(db_err)?);
                }
                out.push(Row::Sqlite(SqliteRow::new(cells)));
            }
            Ok(out)
        })
        .await
    }

    /// Run a row-count statement on the connection, under the same assumption as
    /// [`fetch_on_conn`].
    async fn run_on_conn(sql: &str, params: &[Value]) -> Result<u64> {
        let conn = conn()?;
        let sql = sql.to_string();
        let params: Vec<rusqlite::types::Value> = params.iter().map(value_to_sqlite).collect();
        blocking(move || {
            let guard = conn.lock().map_err(|_| poisoned())?;
            let n = guard
                .execute(&sql, rusqlite::params_from_iter(params))
                .map_err(db_err)?;
            Ok(n as u64)
        })
        .await
    }

    /// Issue a parameterless transaction-control statement (`BEGIN`, `COMMIT`,
    /// `SAVEPOINT …`, `ROLLBACK …`) on the connection without touching
    /// [`TX_LOCK`]: the transaction frame already holds it.
    async fn control(sql: &str) -> Result<()> {
        let conn = conn()?;
        let sql = sql.to_string();
        blocking(move || {
            let guard = conn.lock().map_err(|_| poisoned())?;
            guard.execute_batch(&sql).map_err(db_err)
        })
        .await
    }

    /// Run `f` inside a SQLite transaction. The outer call holds [`TX_LOCK`] for
    /// the whole scope and brackets the closure with `BEGIN` …
    /// `COMMIT`/`ROLLBACK`; a nested call is a `SAVEPOINT`, undoing only its own
    /// work on `Err` while the outer transaction stays usable.
    pub(super) async fn atomic<F, Fut, T>(f: F) -> Result<T>
    where
        F: FnOnce(Tx) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        if in_tx() {
            // A savepoint: already holding the connection via the outer frame.
            let state = SQLITE_TX.with(|s| s.clone());
            let depth = {
                let mut d = state.depth.lock().map_err(|_| poisoned())?;
                *d += 1;
                *d
            };
            let name = savepoint_name(depth);
            control(&format!("SAVEPOINT {name}")).await?;
            let outcome = f(Tx { depth }).await;
            {
                let mut d = state.depth.lock().map_err(|_| poisoned())?;
                *d -= 1;
            }
            match outcome {
                Ok(v) => {
                    control(&format!("RELEASE SAVEPOINT {name}")).await?;
                    Ok(v)
                }
                Err(e) => {
                    control(&format!("ROLLBACK TO SAVEPOINT {name}")).await?;
                    control(&format!("RELEASE SAVEPOINT {name}")).await?;
                    Err(e)
                }
            }
        } else {
            // The outermost transaction. Hold the connection lock for the scope.
            let conn = conn()?;
            let guard = tx_lock().lock_owned().await;
            control("BEGIN").await?;

            // On a panic, async unwinding cannot `.await` a rollback — but SQLite
            // is synchronous, so a best-effort `ROLLBACK` in `Drop` reaches the
            // connection and leaves it clean for the next transaction. Disarmed
            // once control returns normally.
            let mut panic_guard = RollbackOnPanic {
                conn: conn.clone(),
                armed: true,
            };

            let state = SqliteTxState {
                depth: Arc::new(Mutex::new(1)),
            };
            let outcome = SQLITE_TX.scope(state, f(Tx { depth: 1 })).await;
            panic_guard.armed = false;

            let result = match outcome {
                Ok(v) => control("COMMIT").await.map(|()| v),
                Err(e) => {
                    // The rollback's own failure must not hide the error that
                    // caused it; that is the one the caller needs to read.
                    let _ = control("ROLLBACK").await;
                    Err(e)
                }
            };
            drop(guard);
            result
        }
    }

    /// Rolls the connection back synchronously if dropped while armed — which
    /// only happens on a panic unwinding out of the transaction's closure.
    struct RollbackOnPanic {
        conn: Arc<Mutex<rusqlite::Connection>>,
        armed: bool,
    }

    impl Drop for RollbackOnPanic {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            // No `spawn_blocking` can hold the connection `Mutex` at this point:
            // the panic is unwinding the async task frame itself, so a plain lock
            // and a synchronous `ROLLBACK` are safe and best-effort.
            if let Ok(guard) = self.conn.lock() {
                let _ = guard.execute_batch("ROLLBACK");
            }
        }
    }

    /// Run a blocking SQLite closure on tokio's blocking pool, flattening the
    /// join error into the ORM's.
    async fn blocking<T, F>(f: F) -> Result<T>
    where
        F: FnOnce() -> Result<T> + Send + 'static,
        T: Send + 'static,
    {
        tokio::task::spawn_blocking(f)
            .await
            .map_err(|e| Error::Db(format!("the SQLite task failed: {e}")))?
    }

    fn db_err(e: rusqlite::Error) -> Error {
        Error::Db(e.to_string())
    }

    fn poisoned() -> Error {
        Error::Db("the SQLite connection lock was poisoned".into())
    }
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
///
/// The backend is the DSN's: PostgreSQL takes a dedicated pooled connection and
/// routes through `CURRENT_TX`; SQLite holds its single shared connection for
/// the scope and routes through the parallel `sqlite` transaction path. Neither
/// closure signature changes, so `atomic(|tx| …)` reads the same on both.
pub async fn atomic<F, Fut, T>(f: F) -> Result<T>
where
    F: FnOnce(Tx) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    match backend()? {
        Backend::Postgres => atomic_pg(f).await,
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => sqlite::atomic(f).await,
        #[cfg(not(feature = "sqlite"))]
        Backend::Sqlite => Err(sqlite_disabled()),
    }
}

/// The PostgreSQL transaction path: a dedicated pooled connection in a task-local,
/// `BEGIN`/`SAVEPOINT` issued directly. See the module docs for why.
async fn atomic_pg<F, Fut, T>(f: F) -> Result<T>
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

    #[tokio::test]
    async fn raw_execute_is_refused_in_a_read_only_session() {
        // The read-only guard must cover the escape hatch too (ADR 0005), or a
        // read-only session is only read-only until someone drops to raw SQL.
        set_read_only(true);
        let err = sql_execute("delete from whatever", &[]).await.unwrap_err();
        assert!(matches!(err, Error::ReadOnly("sql_execute")), "{err}");
        set_read_only(false);
    }

    #[test]
    fn a_transaction_handle_knows_whether_it_is_a_savepoint() {
        assert!(!Tx { depth: 1 }.is_savepoint());
        assert!(Tx { depth: 2 }.is_savepoint());
        assert_eq!(Tx { depth: 3 }.depth(), 3);
    }

    #[test]
    fn a_short_statement_is_logged_whole_and_trimmed() {
        let s = summarize_sql("  SELECT 1  ");
        assert_eq!(s, "SELECT 1");
        assert!(matches!(s, Cow::Borrowed(_)), "no allocation for short SQL");
    }

    #[test]
    fn a_long_statement_is_truncated_on_a_char_boundary_and_tagged() {
        // A multi-byte char straddling the cut must not panic or split a scalar.
        let long = format!("SELECT '{}'", "é".repeat(SQL_LOG_MAX));
        let s = summarize_sql(&long);
        assert!(s.ends_with(&format!("[{} bytes]", long.len())), "{s}");
        assert!(s.contains('…'), "{s}");
        // Still valid UTF-8 up to the ellipsis — the assertion is that building
        // it did not panic, which a mid-char cut would have.
        assert!(s.len() < long.len());
    }

    #[test]
    fn pool_stats_serializes_to_the_documented_shape() {
        // The public snapshot type must round-trip through serde with stable,
        // metrics-friendly field names and no deadpool type leaking out.
        let snap = PoolStats {
            max_size: 8,
            size: 3,
            available: 1,
            waiting: 2,
        };
        let json = serde_json::to_string(&snap).expect("serialize PoolStats");
        assert_eq!(json, r#"{"max_size":8,"size":3,"available":1,"waiting":2}"#);
        let back: PoolStats = serde_json::from_str(&json).expect("deserialize PoolStats");
        assert_eq!(back, snap);
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
