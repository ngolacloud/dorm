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
/// An optional read-replica DSN. When set, the process-default [`Database`]
/// routes the reads DORM compiles (`StatementKind::Read`) to it, while writes —
/// and anything inside a transaction — stay on [`DSN_ENV`]'s primary. Absent,
/// there is no replica and every statement runs on the primary, unchanged.
pub const REPLICA_DSN_ENV: &str = "DELONIX_ORM_REPLICA_DSN";
/// Ceiling on pooled connections.
pub const POOL_MAX_ENV: &str = "DELONIX_ORM_POOL_MAX";
/// How long a caller waits for a connection before giving up.
pub const POOL_ACQUIRE_TIMEOUT_MS_ENV: &str = "DELONIX_ORM_POOL_ACQUIRE_TIMEOUT_MS";
/// Statements slower than this many milliseconds are logged at `warn` (ADR 0008).
/// Absent — or zero — disables the slow-query log entirely, which is the default:
/// production opts *in* to the noise, it is never surprised by it.
pub const SLOW_QUERY_MS_ENV: &str = "DELONIX_ORM_SLOW_QUERY_MS";
/// Server-side `statement_timeout`, in milliseconds. `0`/unset means no timeout
/// (ADR 0004). When set, PostgreSQL cancels an over-running statement and the
/// ORM surfaces [`Error::Timeout`].
pub const STATEMENT_TIMEOUT_MS_ENV: &str = "DELONIX_ORM_STATEMENT_TIMEOUT_MS";
/// Overrides the DSN's `sslmode` (ADR 0004): `disable`, `require`, `verify-full`.
pub const SSLMODE_ENV: &str = "DELONIX_ORM_SSLMODE";
/// Overrides the DSN's `sslrootcert`: a PEM file of roots to trust.
pub const SSLROOTCERT_ENV: &str = "DELONIX_ORM_SSLROOTCERT";
/// When truthy (`1`/`true`/`yes`/`on`), statements are sent unprepared —
/// `conn.query(sql, …)` instead of `prepare_cached` (ADR 0007).
///
/// This is the switch for **PgBouncer in `transaction` pooling mode**: there a
/// named prepared statement lives on a server connection that PgBouncer may
/// hand to a different client on the next transaction, so a cached handle comes
/// back as `prepared statement "sN" does not exist` (`26000`). Unprepared
/// execution has no named handle to lose. It costs a Parse round trip per call —
/// the price of running behind a transaction-mode pooler (DBA-03/H2).
pub const NO_PREPARE_ENV: &str = "DELONIX_ORM_NO_PREPARE";
/// How a connection is checked before a checkout hands it out (ADR 0015):
/// `verified` (the default — a liveness round trip, so a connection a database
/// restart or `pg_terminate_backend` killed is detected and replaced rather than
/// handed to a doomed query), `fast` (no round trip, lowest latency), or `clean`
/// (`DISCARD ALL` as well). The resilient default costs one empty round trip per
/// checkout; `fast` is the opt-out for a latency-critical, stable deployment.
pub const POOL_RECYCLING_ENV: &str = "DELONIX_ORM_POOL_RECYCLING";
/// How many times `acquire` retries a transient connection failure — a database
/// restarting, or a momentarily exhausted pool — before giving up. `0` disables
/// retry. **Only the acquire is retried**, never a statement already in flight:
/// a connection breaking mid-write could have committed, so retrying it might
/// double-apply. An acquire has sent nothing, so retrying it is always safe.
pub const RETRY_MAX_ENV: &str = "DELONIX_ORM_RETRY_MAX";
/// The base backoff between acquire retries, in milliseconds; it doubles each
/// attempt up to a 2 s cap. See [`RETRY_MAX_ENV`].
pub const RETRY_BASE_MS_ENV: &str = "DELONIX_ORM_RETRY_BASE_MS";

const POOL_MAX_DEFAULT: usize = 8;
const POOL_ACQUIRE_TIMEOUT_DEFAULT_MS: u64 = 5_000;
const RETRY_MAX_DEFAULT: u64 = 3;
const RETRY_BASE_MS_DEFAULT: u64 = 50;
/// The ceiling on a single backoff wait, so exponential growth cannot stall a
/// request for minutes.
const RETRY_BACKOFF_CAP_MS: u64 = 2_000;

/// How much SQL a log line carries before it is truncated. Long enough to read
/// the shape of a statement, short enough that a 50-line generated query does
/// not flood the log. The full SQL is never a secret — the *parameters* are, and
/// those are never here (see [`observe`]).
const SQL_LOG_MAX: usize = 300;

/// The process-default database, built lazily from the environment the first
/// time it is needed.
///
/// It is a *default*, not the only source. A caller can build a [`Database`]
/// explicitly and run against it with [`with_session`]; the module never
/// *requires* this global — which is what lets one process serve many databases
/// (a tenant's, a primary/replica pair) instead of the single process-wide pool
/// it used to be locked to.
static DEFAULT_DB: OnceLock<Database> = OnceLock::new();

tokio::task_local! {
    /// The [`Database`] in scope for this task, if any. Set by [`with_session`]
    /// and consulted by every statement before it falls back to `DEFAULT_DB`.
    /// This is what carries a per-session database — a tenant's, or a
    /// primary/replica pair — down to the `db::*` calls without threading a
    /// handle through every `QuerySet` method (the same task-local shape the
    /// transaction routing and the read-only scope already use).
    static SESSION: Arc<Database>;
}

/// Which connection a statement wants: the primary, or a read replica when the
/// active [`Database`] has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Route {
    /// Writes, DDL, raw statements, and everything inside a transaction.
    Primary,
    /// A read DORM compiled (`compiler::StatementKind::Read`); served by the
    /// replica when the active database has one, otherwise by the primary.
    ReadReplica,
}

/// The process-wide read-only default, for the one-process-one-session case (the
/// CLI/shell's `--read-only`). A *session* can override it per task — see
/// [`READ_ONLY_SCOPE`] and [`with_read_only`].
static READ_ONLY: AtomicBool = AtomicBool::new(false);

tokio::task_local! {
    /// A per-session read-only override (B-12 / SEC-03). A task running inside
    /// [`with_read_only`] consults this instead of the process-wide [`READ_ONLY`]
    /// flag, so one request can be read-only while another, concurrent request on
    /// the same process writes — the isolation a global `AtomicBool` cannot give.
    /// Outside any such scope the global default applies, so the CLI/shell is
    /// unaffected.
    static READ_ONLY_SCOPE: bool;
}

tokio::task_local! {
    /// The tenant in scope for this task. A `tenant_scoped` model compiled with
    /// this set gets its tenant predicate (and insert value) injected by the
    /// scope seam; compiled without it — and outside [`SYSTEM_SCOPE`] — it is
    /// refused with [`Error::TenantScopeMissing`] (ADR 0021). Task-scoped, like
    /// the read-only override, so concurrent sessions never leak into each other.
    static TENANT: Value;
}

tokio::task_local! {
    /// An explicit, audited bypass of tenant scoping — for the rare
    /// cross-tenant admin path. Set by [`with_system_scope`], which emits a
    /// `tracing` event on entry so a bypass is never silent (ADR 0021).
    static SYSTEM_SCOPE: bool;
}

/// The slow-query threshold, read from the environment exactly once.
///
/// Like the pool sizing, this is process configuration set at launch, not a
/// per-call knob — so it is resolved on first use and cached, keeping the hot
/// path free of an env lookup (and its lock + allocation) on every statement.
/// `None` means the slow-query log is off; `Some(d)` is the threshold.
static SLOW_QUERY: OnceLock<Option<Duration>> = OnceLock::new();

/// Whether to skip `prepare_cached`, read from the environment exactly once.
///
/// Process configuration set at launch (for a transaction-mode pooler), not a
/// per-call knob, so it is resolved on first use and cached off the hot path —
/// like [`SLOW_QUERY`]. See [`NO_PREPARE_ENV`].
static NO_PREPARE: OnceLock<bool> = OnceLock::new();

/// `true` when statements must be sent unprepared (see [`NO_PREPARE_ENV`]).
fn no_prepare() -> bool {
    *NO_PREPARE.get_or_init(|| {
        std::env::var(NO_PREPARE_ENV)
            .map(|v| {
                matches!(
                    v.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                )
            })
            .unwrap_or(false)
    })
}

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

/// A non-negative millisecond count from the environment, `default` when unset
/// or unparseable. Unlike [`env_usize`], `0` is a legitimate value here — it is
/// how `statement_timeout` is turned off.
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Mark this **process** as read-only. Every write refuses before it reaches SQL.
///
/// The session is *also* opened `READ ONLY` at the server, which is the real
/// guard; this flag exists so the refusal names the operation instead of
/// surfacing a PostgreSQL error about a read-only transaction.
///
/// This is the process-wide default, for the one-process-one-session case (the
/// CLI/shell's `--read-only`). A server handling many sessions on one process
/// wants [`with_read_only`] instead, which scopes the flag to one task so
/// concurrent sessions do not leak into each other (B-12 / SEC-03).
pub fn set_read_only(yes: bool) {
    READ_ONLY.store(yes, Ordering::SeqCst);
}

/// Run `fut` with the read-only state forced to `yes` for this task alone,
/// overriding the process-wide [`set_read_only`] default.
///
/// This is the per-session form: two concurrent requests can each wrap their work
/// in `with_read_only` with different values and neither sees the other's — the
/// isolation the global flag cannot provide. It applies to the whole async
/// subtree the future awaits (transactions included), but not to tasks spawned
/// off it, which start their own task-local scope — a spawned task is a new
/// session.
pub async fn with_read_only<F, T>(yes: bool, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    READ_ONLY_SCOPE.scope(yes, fut).await
}

/// Run `fut` with `tenant` as the active tenant for this task (ADR 0021).
///
/// Inside the scope, every `tenant_scoped` model the ORM compiles is constrained
/// to this tenant automatically — the predicate on reads/updates/deletes, the
/// column value on inserts — so a forgotten `.filter` cannot cross tenants. Like
/// [`with_read_only`], it covers the whole async subtree but not tasks spawned
/// off it.
pub async fn with_tenant<F, T>(tenant: impl crate::value::ToValue, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    TENANT.scope(tenant.to_value(), fut).await
}

/// Run `fut` with tenant scoping **bypassed** — the audited cross-tenant admin
/// escape (ADR 0021). Emits a `tracing` event on entry so the bypass is on the
/// record; prefer [`with_tenant`] for everything that is not deliberately global.
pub async fn with_system_scope<F, T>(fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    tracing::warn!(target: "dorm::tenant", "entering with_system_scope: tenant isolation bypassed");
    SYSTEM_SCOPE.scope(true, fut).await
}

/// The tenant in scope for this task, if any ([`with_tenant`]).
pub fn current_tenant() -> Option<Value> {
    TENANT.try_with(|v| v.clone()).ok()
}

/// Whether this task is running under [`with_system_scope`] (tenant scoping off).
pub fn is_system_scope() -> bool {
    SYSTEM_SCOPE.try_with(|&b| b).unwrap_or(false)
}

/// Whether the current context is read-only, most specific wins: the per-task
/// scope ([`with_read_only`], B-12) overrides the session's [`Database`] default
/// ([`with_session`]), which overrides the process-wide [`set_read_only`] flag.
pub fn is_read_only() -> bool {
    if let Ok(yes) = READ_ONLY_SCOPE.try_with(|&yes| yes) {
        return yes;
    }
    if let Ok(yes) = SESSION.try_with(|db| db.read_only) {
        return yes;
    }
    READ_ONLY.load(Ordering::SeqCst)
}

/// Refuse a write in a read-only session.
pub fn guard_write(op: &'static str) -> Result<()> {
    if is_read_only() {
        return Err(Error::ReadOnly(op));
    }
    Ok(())
}

/// The GUCs [`set_local`] is allowed to set. A closed list, because the key of a
/// `SET` cannot be a bound parameter — it is spliced into the statement — so it
/// must never come freely from a caller. Values always bind (ADR 0023).
///
/// `dorm.tenant` is the RLS GUC the tenant-scope policies read (ADR 0021); the
/// timeout GUCs are what online migrations set per transaction (ADR 0022).
pub const SET_LOCAL_ALLOWED: &[&str] = &[
    "dorm.tenant",
    "lock_timeout",
    "statement_timeout",
    "idle_in_transaction_session_timeout",
];

/// Set a transaction-local GUC safely: the key is checked against
/// [`SET_LOCAL_ALLOWED`] and the value is **bound**, via
/// `set_config(key, value, is_local = true)` rather than `SET LOCAL key = value`
/// (which takes no parameter). Must run inside a transaction — a `SET LOCAL`
/// outside one is a no-op the caller would not expect.
///
/// This is the one mechanism the RLS tenant GUC (ADR 0021) and the migration
/// lock timeout (ADR 0022) go through, so the allow-list is enforced in exactly
/// one place.
pub async fn set_local(key: &str, value: &str) -> Result<()> {
    if !SET_LOCAL_ALLOWED.contains(&key) {
        return Err(Error::Config(format!(
            "`{key}` is not a settable GUC (allowed: {})",
            SET_LOCAL_ALLOWED.join(", ")
        )));
    }
    if !in_transaction() {
        return Err(Error::Config(format!(
            "set_local(`{key}`) must run inside a transaction; outside one a SET LOCAL has no effect"
        )));
    }
    // `set_config`'s first argument is the GUC name; splicing it is safe because
    // it came from the allow-list above, never from the caller. The value binds.
    let sql = format!("SELECT set_config('{key}', $1, true)");
    fetch(&sql, &[Value::Text(value.to_string())]).await?;
    Ok(())
}

/// A database: a primary connection pool, an optional read replica, and the
/// read-only default for sessions that run against it.
///
/// This is the handle that replaced the single process-wide pool. A process can
/// hold as many as it needs — one per tenant, a primary/replica pair — and reach
/// one for a unit of work with [`with_session`], instead of every `db::*` call
/// implicitly meaning the one global pool. The `DEFAULT_DB` built from the
/// environment is just the most common case of this, kept so the embedded
/// single-database path needs no session at all.
///
/// # The pools belong to one runtime
///
/// `tokio-postgres` drives each connection from a background task, so a pooled
/// connection is owned by the runtime that created it. **The process must keep
/// one runtime alive for as long as it uses these pools.** Drop that runtime and
/// every connection dies with it; the next borrow comes back `connection
/// closed`. It matters for the shell: the synchronous REPL builds its runtime
/// once and `block_on`s against that same one for the whole session. A runtime
/// per statement would work for exactly one statement — it is what the
/// integration tests did first, and five of them failed on it.
pub struct Database {
    primary: Pool,
    /// Present when a replica DSN was configured; reads route here.
    replica: Option<Pool>,
    /// The primary DSN, kept so backend/dialect detection can consult the active
    /// database rather than only the process environment.
    dsn: String,
    /// The read-only default for sessions on this database. A task can still
    /// override it per request with [`with_read_only`] (B-12).
    read_only: bool,
}

impl Database {
    /// Connect a database from explicit DSNs: a required primary and an optional
    /// read replica. `read_only` becomes the default for sessions that run
    /// against it (overridable per task by [`with_read_only`]).
    ///
    /// This is the handle a server builds per tenant, or once with a
    /// primary/replica pair; it needs no process environment set, which is what
    /// frees the module from requiring the global pool.
    pub fn connect(primary_dsn: &str, replica_dsn: Option<&str>, read_only: bool) -> Result<Self> {
        let primary = build_pool(primary_dsn)?;
        let replica = match replica_dsn {
            Some(r) if !r.trim().is_empty() => Some(build_pool(r)?),
            _ => None,
        };
        Ok(Database {
            primary,
            replica,
            dsn: primary_dsn.to_string(),
            read_only,
        })
    }

    /// Build the process-default database from the environment: [`DSN_ENV`] for
    /// the primary and [`REPLICA_DSN_ENV`] for the optional replica.
    fn from_env() -> Result<Self> {
        let primary = dsn()?;
        let replica = std::env::var(REPLICA_DSN_ENV)
            .ok()
            .filter(|r| !r.trim().is_empty());
        Database::connect(&primary, replica.as_deref(), false)
    }

    /// The pool a route resolves to. A [`Route::ReadReplica`] picks the replica
    /// only when one exists **and** no transaction is open: a transaction already
    /// holds its own connection and must not be split across primary and replica,
    /// and it is the one place a read must see its own uncommitted writes.
    fn pool_for(&self, route: Route) -> &Pool {
        match route {
            Route::ReadReplica if !in_transaction() => {
                self.replica.as_ref().unwrap_or(&self.primary)
            }
            _ => &self.primary,
        }
    }
}

/// Build one connection pool for a DSN. This is the former body of `pool()`,
/// lifted so a [`Database`] can build a primary and a replica the same way.
fn build_pool(raw: &str) -> Result<Pool> {
    // `tokio_postgres::Config` rejects `sslmode=verify-full` and does not know
    // `sslrootcert`, so the TLS keys DORM handles itself are pulled out before
    // the parse; verification is done by the connector, not the driver.
    let (cleaned, dsn_mode, dsn_root) = extract_tls_params(raw);
    let mut config: Config = cleaned
        .parse()
        .map_err(|e| Error::Config(format!("the DSN is not usable: {e}")))?;

    // Server-side `statement_timeout` (ADR 0004): injected as a libpq option so
    // PostgreSQL itself cancels an over-running statement. `0` leaves it off.
    if let Some(opts) =
        merge_statement_timeout(config.get_options(), env_u64(STATEMENT_TIMEOUT_MS_ENV, 0))
    {
        config.options(opts);
    }

    // TLS selection: env overrides the DSN, which overrides "disable".
    let mode = std::env::var(SSLMODE_ENV).ok().or(dsn_mode);
    let root = std::env::var(SSLROOTCERT_ENV).ok().or(dsn_root);
    let requirement = ssl_requirement(mode.as_deref())?;

    let manager = build_manager(config, requirement, root.as_deref())?;
    Pool::builder(manager)
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
        .map_err(|e| Error::Pool(e.to_string()))
}

/// The process-default database, built from the environment on first use.
fn default_db() -> Result<&'static Database> {
    if let Some(d) = DEFAULT_DB.get() {
        return Ok(d);
    }
    let built = Database::from_env()?;
    Ok(DEFAULT_DB.get_or_init(|| built))
}

/// Run `fut` with `db` as the database in scope for the task, so every `db::*`
/// statement it makes — queries, writes, transactions — runs against `db`
/// instead of the process default. Concurrent sessions on different databases do
/// not see each other's: a spawned task starts outside the scope and falls back
/// to the default, exactly as the read-only scope and the transaction scope do.
pub async fn with_session<F, T>(db: Arc<Database>, fut: F) -> T
where
    F: std::future::Future<Output = T>,
{
    SESSION.scope(db, fut).await
}

/// The primary pool of the process-default database.
///
/// Kept for callers that want the process pool directly — the pool-stats gauge,
/// chiefly. It is deliberately the *default*: a statement does not go through
/// here, it goes through `acquire`, which honours the session in scope. This
/// returns the global so the embedded single-database path is unchanged.
pub fn pool() -> Result<&'static Pool> {
    Ok(&default_db()?.primary)
}

// --------------------------------------------------------------------- TLS
//
// ADR 0004. TLS is feature-gated: `tls-rustls` (default, pure Rust) and
// `tls-openssl` (system libssl). The DSN's `sslmode` picks the strictness; the
// driver only learns "request TLS or not", because the certificate check lives
// in the connector, not in `tokio-postgres`.

/// What the DSN asks of the connection's TLS, from its `sslmode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslRequirement {
    /// Plaintext — `NoTls`, whatever TLS features are compiled in.
    Disable,
    /// Encrypt, but do not verify the server certificate (libpq `require`).
    Require,
    /// Encrypt and verify the certificate chain **and** the hostname.
    VerifyFull,
}

/// Map an `sslmode` token to a [`SslRequirement`], or refuse one this release
/// does not implement. `None` and the opportunistic modes are `Disable`: DORM
/// never silently half-encrypts — ask for `require` or `verify-full` to get TLS.
pub fn ssl_requirement(mode: Option<&str>) -> Result<SslRequirement> {
    match mode.map(|m| m.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("disable") | Some("prefer") | Some("allow") => {
            Ok(SslRequirement::Disable)
        }
        Some("require") => Ok(SslRequirement::Require),
        Some("verify-full") => Ok(SslRequirement::VerifyFull),
        Some("verify-ca") => Err(Error::Tls(
            "sslmode=verify-ca is not implemented yet; use verify-full (chain + \
             hostname) or require (no verification)"
                .into(),
        )),
        Some(other) => Err(Error::Tls(format!(
            "unknown sslmode `{other}`; use disable, require, or verify-full"
        ))),
    }
}

/// Pull the TLS keys DORM handles itself (`sslmode`, `sslrootcert`) out of the
/// DSN, returning `(dsn_without_them, sslmode, sslrootcert)`. Handles both the
/// libpq keyword form (`host=… sslmode=require`) and the URL query form
/// (`postgres://…?sslmode=verify-full`). Pure, so it is unit-tested.
fn extract_tls_params(dsn: &str) -> (String, Option<String>, Option<String>) {
    let (mut mode, mut root) = (None, None);
    let take = |key: &str, tok: &str, slot: &mut Option<String>| -> bool {
        if let Some(v) = tok.strip_prefix(key) {
            *slot = Some(v.to_string());
            true
        } else {
            false
        }
    };

    if let Some((base, query)) = dsn.split_once('?').filter(|_| dsn.contains("://")) {
        let kept: Vec<&str> = query
            .split('&')
            .filter(|pair| {
                !(take("sslmode=", pair, &mut mode)
                    || take("sslrootcert=", pair, &mut root)
                    || pair.is_empty())
            })
            .collect();
        let rebuilt = if kept.is_empty() {
            base.to_string()
        } else {
            format!("{base}?{}", kept.join("&"))
        };
        (rebuilt, mode, root)
    } else {
        let kept: Vec<&str> = dsn
            .split_whitespace()
            .filter(|tok| {
                !(take("sslmode=", tok, &mut mode) || take("sslrootcert=", tok, &mut root))
            })
            .collect();
        (kept.join(" "), mode, root)
    }
}

/// Fold a server-side `statement_timeout` into libpq `options`. `ms == 0` is
/// "no timeout" and leaves the options untouched. Pure, so it is unit-tested.
fn merge_statement_timeout(options: Option<&str>, ms: u64) -> Option<String> {
    if ms == 0 {
        return options.map(str::to_string);
    }
    let clause = format!("-c statement_timeout={ms}");
    Some(match options {
        Some(existing) if !existing.trim().is_empty() => format!("{existing} {clause}"),
        _ => clause,
    })
}

/// The connection-recycling strategy from the environment (ADR 0015), defaulting
/// to the resilient `Verified`. See [`POOL_RECYCLING_ENV`].
fn recycling_method() -> RecyclingMethod {
    match std::env::var(POOL_RECYCLING_ENV)
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Ok("fast") => RecyclingMethod::Fast,
        Ok("clean") => RecyclingMethod::Clean,
        // `verified` by name, and also the default for anything unset or
        // unrecognised: resilience is the safe thing to assume.
        _ => RecyclingMethod::Verified,
    }
}

/// Exponential backoff for the n-th acquire retry (0-based), capped. Pure, so the
/// schedule is unit-tested. Attempt 0 waits `base`, 1 waits `2×base`, and so on,
/// never past [`RETRY_BACKOFF_CAP_MS`].
fn backoff_delay(base_ms: u64, attempt: u32) -> Duration {
    let factor = 1u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let ms = base_ms.saturating_mul(factor).min(RETRY_BACKOFF_CAP_MS);
    Duration::from_millis(ms)
}

/// Build the pool's `Manager`, choosing the TLS connector from the requirement
/// and the compiled features. `deadpool`'s `Manager` is not generic over the TLS
/// type — it erases it — so every arm returns the same `Manager` and the process
/// pool stays one concrete type.
// The `return`s are needed: exactly one of the TLS arms below compiles per
// feature set, so to the lint each looks like a needless tail return.
#[allow(clippy::needless_return)]
fn build_manager(
    mut config: Config,
    requirement: SslRequirement,
    root: Option<&str>,
) -> Result<Manager> {
    // How a connection is checked on checkout (ADR 0015). `Verified` (the default)
    // does a liveness round trip, so a connection killed by a restart or
    // `pg_terminate_backend` is detected and replaced rather than handed to a
    // doomed query; `fast` opts out of the round trip. Either is safe: a
    // connection is only ever returned to the pool with no transaction open
    // (`atomic` takes it out instead of returning a dirty one).
    let mgr_config = ManagerConfig {
        recycling_method: recycling_method(),
    };

    if requirement == SslRequirement::Disable {
        config.ssl_mode(tokio_postgres::config::SslMode::Disable);
        return Ok(Manager::from_config(config, NoTls, mgr_config));
    }

    // TLS wanted: make the driver actually offer it. The connector decides how
    // strictly the certificate is checked.
    config.ssl_mode(tokio_postgres::config::SslMode::Require);

    #[cfg(feature = "tls-rustls")]
    {
        return Ok(Manager::from_config(
            config,
            tls_rustls::connector(requirement, root)?,
            mgr_config,
        ));
    }
    #[cfg(all(feature = "tls-openssl", not(feature = "tls-rustls")))]
    {
        return Ok(Manager::from_config(
            config,
            tls_openssl::connector(requirement, root)?,
            mgr_config,
        ));
    }
    #[cfg(not(any(feature = "tls-rustls", feature = "tls-openssl")))]
    {
        let _ = (root, mgr_config);
        Err(Error::Tls(
            "the DSN asks for TLS (sslmode=require or verify-full) but this build \
             has no TLS backend; rebuild dorm-core with --features tls-rustls (or \
             tls-openssl), or set sslmode=disable"
                .into(),
        ))
    }
}

/// The rustls TLS backend (default). Pure Rust over the ring provider.
#[cfg(feature = "tls-rustls")]
mod tls_rustls {
    use std::sync::Arc;

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
    use tokio_postgres_rustls::MakeRustlsConnect;

    use super::{Error, Result, SslRequirement};

    pub(super) fn connector(req: SslRequirement, root: Option<&str>) -> Result<MakeRustlsConnect> {
        // `ClientConfig::builder()` reads the process-default crypto provider;
        // install ring's if nothing installed one already (a second install is
        // an error we can ignore — it means a provider is present).
        let _ = CryptoProvider::install_default(rustls::crypto::ring::default_provider());

        let config = match req {
            SslRequirement::VerifyFull => ClientConfig::builder()
                .with_root_certificates(load_roots(root)?)
                .with_no_client_auth(),
            SslRequirement::Require => ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(NoVerify::new()))
                .with_no_client_auth(),
            SslRequirement::Disable => unreachable!("Disable never reaches the TLS connector"),
        };
        Ok(MakeRustlsConnect::new(config))
    }

    fn load_roots(root: Option<&str>) -> Result<RootCertStore> {
        let mut store = RootCertStore::empty();
        match root {
            Some(path) => {
                let pem = std::fs::read(path)
                    .map_err(|e| Error::Tls(format!("cannot read sslrootcert `{path}`: {e}")))?;
                for cert in rustls_pemfile::certs(&mut &pem[..]) {
                    let cert =
                        cert.map_err(|e| Error::Tls(format!("bad certificate in `{path}`: {e}")))?;
                    store.add(cert).map_err(|e| {
                        Error::Tls(format!("cannot trust a certificate in `{path}`: {e}"))
                    })?;
                }
            }
            None => store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
        }
        Ok(store)
    }

    /// `sslmode=require`: the handshake still proves the server holds the key for
    /// the certificate it presents (the signatures are verified), but the chain
    /// and hostname are not checked. That is exactly libpq's `require`.
    #[derive(Debug)]
    struct NoVerify(Arc<CryptoProvider>);

    impl NoVerify {
        fn new() -> Self {
            Self(Arc::new(rustls::crypto::ring::default_provider()))
        }
    }

    impl ServerCertVerifier for NoVerify {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> std::result::Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls12_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.0.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }
}

/// The OpenSSL TLS backend (opt-in). Uses the system libssl.
#[cfg(all(feature = "tls-openssl", not(feature = "tls-rustls")))]
mod tls_openssl {
    use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
    use postgres_openssl::MakeTlsConnector;

    use super::{Error, Result, SslRequirement};

    pub(super) fn connector(req: SslRequirement, root: Option<&str>) -> Result<MakeTlsConnector> {
        let mut builder = SslConnector::builder(SslMethod::tls())
            .map_err(|e| Error::Tls(format!("cannot build the TLS connector: {e}")))?;
        if let Some(path) = root {
            builder
                .set_ca_file(path)
                .map_err(|e| Error::Tls(format!("cannot load sslrootcert `{path}`: {e}")))?;
        }
        match req {
            SslRequirement::Require => builder.set_verify(SslVerifyMode::NONE),
            SslRequirement::VerifyFull => builder.set_verify(SslVerifyMode::PEER),
            SslRequirement::Disable => unreachable!("Disable never reaches the TLS connector"),
        }
        let mut make = MakeTlsConnector::new(builder.build());
        if req == SslRequirement::Require {
            // `require` must not check the hostname either; the connector would
            // otherwise verify it and reject a mismatch.
            make.set_callback(|cfg, _domain| {
                cfg.set_verify_hostname(false);
                Ok(())
            });
        }
        Ok(make)
    }
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

/// Acquire a connection for `route` from the database in scope — the session's
/// if [`with_session`] set one, otherwise the process default. Holding the
/// session `Arc` across the `.await` keeps its pools alive for the borrow.
/// Acquire a connection for `route`, retrying a transient failure with backoff
/// (ADR 0015). A pool error means nothing was sent on any connection yet — the
/// database is restarting, or the pool is momentarily exhausted — so retrying is
/// always safe (unlike a statement in flight, which `acquire` never retries). The
/// `Verified` recycling above already replaces a *dead* pooled connection
/// transparently; this retry covers the window where a fresh connection cannot be
/// made yet because the server is still coming back.
async fn acquire(route: Route) -> Result<Object> {
    let max = env_u64(RETRY_MAX_ENV, RETRY_MAX_DEFAULT);
    let base = env_u64(RETRY_BASE_MS_ENV, RETRY_BASE_MS_DEFAULT);
    let mut attempt: u32 = 0;
    loop {
        match acquire_once(route).await {
            Ok(conn) => return Ok(conn),
            Err(e) => {
                if u64::from(attempt) >= max {
                    return Err(e);
                }
                tracing::warn!(
                    target: "dorm::db",
                    attempt = attempt + 1,
                    max,
                    error = %e,
                    "retrying connection acquisition after a transient failure"
                );
                tokio::time::sleep(backoff_delay(base, attempt)).await;
                attempt += 1;
            }
        }
    }
}

/// One acquisition from the database in scope — the session's pool, or the
/// process default's.
async fn acquire_once(route: Route) -> Result<Object> {
    if let Ok(db) = SESSION.try_with(|d| d.clone()) {
        return db
            .pool_for(route)
            .get()
            .await
            .map_err(|e| Error::Pool(format!("could not acquire a connection: {e}")));
    }
    default_db()?
        .pool_for(route)
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
/// named statement per connection (deadpool's `StatementCache`, ADR 0007), so
/// the same SQL — and the ORM emits the same text with `$n` placeholders for a
/// given query — pays Parse once and is Bind/Execute-only thereafter. The win
/// grows with repetition, which is the ORM's whole access pattern.
pub async fn query(sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<PgRow>> {
    query_routed(sql, params, Route::Primary).await
}

/// Run a read on a read replica when the active [`Database`] has one — otherwise
/// identical to [`query`]. This is the entry the typed read funnel uses for a
/// `StatementKind::Read`; a transaction in scope keeps it on the primary (a
/// transaction must read its own writes), as `Database::pool_for` enforces.
pub async fn query_read(sql: &str, params: &[&(dyn ToSql + Sync)]) -> Result<Vec<PgRow>> {
    query_routed(sql, params, Route::ReadReplica).await
}

async fn query_routed(
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
    route: Route,
) -> Result<Vec<PgRow>> {
    // Observed end to end (ADR 0008): the clock spans Parse-or-cache-hit plus
    // Bind/Execute, which is the latency the caller actually pays. `in_tx` is
    // read up front so the event records the routing even on the error path.
    let in_tx = in_transaction();
    let start = Instant::now();
    let result = query_inner(sql, params, route).await;
    observe(
        "query",
        sql,
        start.elapsed(),
        result.as_ref().ok().map(|rows| rows.len() as u64),
        in_tx,
    );
    result
}

async fn query_inner(
    sql: &str,
    params: &[&(dyn ToSql + Sync)],
    route: Route,
) -> Result<Vec<PgRow>> {
    if let Ok(tx) = CURRENT_TX.try_with(|t| t.clone()) {
        let guard = tx.lock().await;
        let conn = guard.conn.as_ref().ok_or_else(|| {
            Error::Db("the transaction's connection is gone; it was rolled back".into())
        })?;
        if no_prepare() {
            return conn.query(sql, params).await.map_err(from_pg);
        }
        let stmt = conn.prepare_cached(sql).await.map_err(from_pg)?;
        return conn.query(&stmt, params).await.map_err(from_pg);
    }
    let conn = acquire(route).await?;
    if no_prepare() {
        return conn.query(sql, params).await.map_err(from_pg);
    }
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
        if no_prepare() {
            return conn.execute(sql, params).await.map_err(from_pg);
        }
        let stmt = conn.prepare_cached(sql).await.map_err(from_pg)?;
        return conn.execute(&stmt, params).await.map_err(from_pg);
    }
    // Writes always run on the primary.
    let conn = acquire(Route::Primary).await?;
    if no_prepare() {
        return conn.execute(sql, params).await.map_err(from_pg);
    }
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
    /// MySQL / MariaDB (ADR 0018), opt-in behind the `mysql` feature.
    MySql,
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
    } else if lower.starts_with("mysql:") || lower.starts_with("mariadb:") {
        Backend::MySql
    } else {
        Backend::Postgres
    }
}

/// The DSN of the database in scope — the session's ([`with_session`]) if one is
/// set, otherwise the process environment's. Backend and dialect detection use
/// this so a session on an explicit [`Database`] is classified by *its* DSN, and
/// a process that runs purely through sessions needs no environment DSN at all.
fn active_dsn() -> Result<String> {
    if let Ok(d) = SESSION.try_with(|db| db.dsn.clone()) {
        return Ok(d);
    }
    dsn()
}

/// The configured backend, or an error if no DSN is set.
fn backend() -> Result<Backend> {
    Ok(backend_of(&active_dsn()?))
}

/// The SQL dialect the configured backend speaks.
///
/// Used by the `QuerySet` terminals to compile with `compile_*_on`. It never
/// fails: with no DSN set it answers PostgreSQL, so offline compilation of a
/// query's SQL still works — the missing DSN surfaces later, when the query is
/// actually run.
pub fn dialect() -> Dialect {
    match active_dsn().map(|d| backend_of(&d)) {
        Ok(Backend::Sqlite) => Dialect::Sqlite,
        Ok(Backend::MySql) => Dialect::MySql,
        _ => Dialect::Postgres,
    }
}

/// Whether the active backend supports `cap` (ADR 0011) — a convenience over
/// [`dialect`]`().supports(cap)` for callers outside the compiler, e.g. a
/// migrator deciding whether it may emit `CREATE INDEX CONCURRENTLY`.
pub fn supports(cap: crate::compiler::Capability) -> bool {
    dialect().supports(cap)
}

// -------------------------------------------------------- the backend driver
//
// ADR 0011. `Driver` is the per-statement runtime seam a backend implements:
// given a compiled statement, run it. It is the extension point a non-PG-wire
// engine (MySQL, SQL Server, Oracle) plugs into — it brings its own wire driver,
// parameter binding and row type, and implements these three verbs. The
// transaction *openers* stay backend-split (`atomic_pg`, the `sqlite` module);
// these are the per-statement verbs the typed path funnels through, and the free
// functions below select the driver from the active backend and delegate.

trait Driver {
    /// Run a row-returning statement; `route` lets a read prefer a replica.
    async fn fetch(&self, sql: &str, params: &[Value], route: Route) -> Result<Vec<Row>>;
    /// Run a statement for its affected-row count.
    async fn run(&self, sql: &str, params: &[Value]) -> Result<u64>;
    /// Run one or more parameterless DDL statements.
    async fn run_ddl(&self, ddl: &str) -> Result<()>;
}

/// The PostgreSQL driver: the pooled, replica-aware path built on [`query`]/
/// [`query_read`]/[`execute`], and so on the [`Database`] in scope.
struct PostgresDriver;

impl Driver for PostgresDriver {
    async fn fetch(&self, sql: &str, params: &[Value], route: Route) -> Result<Vec<Row>> {
        let borrowed: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
        let rows = match route {
            Route::ReadReplica => query_read(sql, &borrowed).await?,
            Route::Primary => query(sql, &borrowed).await?,
        };
        Ok(rows.into_iter().map(Row::Pg).collect())
    }

    async fn run(&self, sql: &str, params: &[Value]) -> Result<u64> {
        let borrowed: Vec<&(dyn ToSql + Sync)> =
            params.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
        execute(sql, &borrowed).await
    }

    async fn run_ddl(&self, ddl: &str) -> Result<()> {
        simple(&acquire(Route::Primary).await?, ddl).await
    }
}

/// The SQLite driver: the single shared connection. It has no replica, so `route`
/// does not change where a statement runs.
#[cfg(feature = "sqlite")]
struct SqliteDriver;

#[cfg(feature = "sqlite")]
impl Driver for SqliteDriver {
    async fn fetch(&self, sql: &str, params: &[Value], _route: Route) -> Result<Vec<Row>> {
        sqlite::fetch(sql, params).await
    }

    async fn run(&self, sql: &str, params: &[Value]) -> Result<u64> {
        sqlite::run(sql, params).await
    }

    async fn run_ddl(&self, ddl: &str) -> Result<()> {
        sqlite::run_ddl(ddl).await
    }
}

/// The MySQL driver: its own `mysql_async` pool (ADR 0018). Like SQLite it does
/// not use the `Database` handle, so `route` is ignored (no replica yet).
#[cfg(feature = "mysql")]
struct MySqlDriver;

#[cfg(feature = "mysql")]
impl Driver for MySqlDriver {
    async fn fetch(&self, sql: &str, params: &[Value], _route: Route) -> Result<Vec<Row>> {
        mysql::fetch(sql, params).await
    }

    async fn run(&self, sql: &str, params: &[Value]) -> Result<u64> {
        mysql::run(sql, params).await
    }

    async fn run_ddl(&self, ddl: &str) -> Result<()> {
        mysql::run_ddl(ddl).await
    }
}

/// Run a row-returning statement on the active backend's `Driver`, taking ORM
/// values and handing back backend-agnostic [`Row`]s. Keeps the
/// transaction-in-scope routing (PostgreSQL via `CURRENT_TX`; SQLite via its own
/// task-local), and always runs on the primary.
pub async fn fetch(sql: &str, params: &[Value]) -> Result<Vec<Row>> {
    fetch_routed(sql, params, Route::Primary).await
}

/// Run a read that may be served by a read replica. This is [`fetch`] for a
/// `StatementKind::Read`: on PostgreSQL the active [`Database`]'s replica serves
/// it when there is one (and no transaction is open); SQLite has no replica, so
/// it is identical to [`fetch`] there.
pub async fn fetch_read(sql: &str, params: &[Value]) -> Result<Vec<Row>> {
    fetch_routed(sql, params, Route::ReadReplica).await
}

async fn fetch_routed(sql: &str, params: &[Value], route: Route) -> Result<Vec<Row>> {
    match backend()? {
        Backend::Postgres => PostgresDriver.fetch(sql, params, route).await,
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => SqliteDriver.fetch(sql, params, route).await,
        #[cfg(not(feature = "sqlite"))]
        Backend::Sqlite => Err(sqlite_disabled()),
        #[cfg(feature = "mysql")]
        Backend::MySql => MySqlDriver.fetch(sql, params, route).await,
        #[cfg(not(feature = "mysql"))]
        Backend::MySql => Err(mysql_disabled()),
    }
}

/// Run a statement for its row count on the active backend's `Driver`.
pub async fn run(sql: &str, params: &[Value]) -> Result<u64> {
    match backend()? {
        Backend::Postgres => PostgresDriver.run(sql, params).await,
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => SqliteDriver.run(sql, params).await,
        #[cfg(not(feature = "sqlite"))]
        Backend::Sqlite => Err(sqlite_disabled()),
        #[cfg(feature = "mysql")]
        Backend::MySql => MySqlDriver.run(sql, params).await,
        #[cfg(not(feature = "mysql"))]
        Backend::MySql => Err(mysql_disabled()),
    }
}

/// Run an `INSERT` and report `(rows affected, first generated key)` — the verb
/// the no-`RETURNING` create path uses to learn the keys the database assigned
/// (ADR 0018 Inc. 2b). Must run inside the `atomic` that reloads the rows, since
/// `last_insert_id` is per-connection. Only MySQL reaches it: PostgreSQL and
/// SQLite have `RETURNING`, so their create path reads the row back in the
/// `INSERT` itself and never calls this.
// Without the `mysql` feature no arm consumes the arguments — the only backend
// that reaches `insert` is MySQL.
#[cfg_attr(not(feature = "mysql"), allow(unused_variables))]
pub async fn insert(sql: &str, params: &[Value]) -> Result<(u64, u64)> {
    match backend()? {
        #[cfg(feature = "mysql")]
        Backend::MySql => mysql::insert(sql, params).await,
        #[cfg(not(feature = "mysql"))]
        Backend::MySql => Err(mysql_disabled()),
        Backend::Postgres | Backend::Sqlite => Err(Error::Db(
            "insert()-then-reload is only for a backend without RETURNING; \
             this backend reads the written row back with INSERT … RETURNING"
                .into(),
        )),
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
        Backend::Postgres => PostgresDriver.run_ddl(ddl).await,
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => SqliteDriver.run_ddl(ddl).await,
        #[cfg(not(feature = "sqlite"))]
        Backend::Sqlite => Err(sqlite_disabled()),
        #[cfg(feature = "mysql")]
        Backend::MySql => MySqlDriver.run_ddl(ddl).await,
        #[cfg(not(feature = "mysql"))]
        Backend::MySql => Err(mysql_disabled()),
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

/// The error a MySQL DSN gets in a build that left the `mysql` feature off.
#[cfg(not(feature = "mysql"))]
fn mysql_disabled() -> Error {
    Error::Config(
        "the DSN selects MySQL, but this build has no MySQL backend; \
         rebuild dorm-core with features = [\"mysql\"]"
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

    /// The reader pool for a file-backed database (ADR 0017). Read-only
    /// connections a standalone read checks out, so reads run concurrently with
    /// the writer under WAL instead of serialising on the one connection. `None`
    /// for `:memory:`, which has no file to share and keeps the single connection.
    static READERS: OnceLock<Option<ReaderPool>> = OnceLock::new();

    /// A pool of read-only connections to one file database, bounded by a
    /// semaphore so at most N readers are live at once. Idle connections are kept
    /// and reused rather than reopened per read.
    struct ReaderPool {
        path: String,
        idle: Mutex<Vec<rusqlite::Connection>>,
        permits: tokio::sync::Semaphore,
    }

    /// The reader pool for the configured DSN, or `None` for `:memory:`. Built
    /// once; its size follows the same `DELONIX_ORM_POOL_MAX` as PostgreSQL's.
    fn readers() -> Option<&'static ReaderPool> {
        READERS
            .get_or_init(|| {
                dsn_path(&dsn().ok()?).map(|path| ReaderPool {
                    path,
                    idle: Mutex::new(Vec::new()),
                    permits: tokio::sync::Semaphore::new(super::env_usize(super::POOL_MAX_ENV, 4)),
                })
            })
            .as_ref()
    }

    /// Run a standalone read on a pooled reader connection. Holds a permit for the
    /// query so the pool never exceeds its size, checks out an idle connection (or
    /// opens one), and returns it afterwards — even if the query failed.
    async fn read_pooled(
        pool: &'static ReaderPool,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Row>> {
        let _permit = pool
            .permits
            .acquire()
            .await
            .map_err(|_| Error::Db("the SQLite reader pool is closed".into()))?;
        let conn = match pool.idle.lock().map_err(|_| poisoned())?.pop() {
            Some(c) => c,
            None => open_reader(&pool.path)?,
        };
        let sql = sql.to_string();
        let params: Vec<rusqlite::types::Value> = params.iter().map(value_to_sqlite).collect();
        // The connection moves into the blocking closure and back out, so a failed
        // query still returns it to the pool rather than leaking it.
        let (result, conn) = blocking(move || {
            let rows = query_on(&conn, &sql, params);
            Ok((rows, conn))
        })
        .await?;
        pool.idle.lock().map_err(|_| poisoned())?.push(conn);
        result
    }

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

    /// The file path a SQLite DSN names, or `None` for an in-memory database.
    /// `sqlite::memory:`, `sqlite:` and `:memory:` are in-memory; anything else is
    /// the path after the scheme.
    fn dsn_path(dsn: &str) -> Option<String> {
        let rest = dsn
            .trim()
            .strip_prefix("sqlite:")
            .or_else(|| dsn.trim().strip_prefix("file:"))
            .unwrap_or_else(|| dsn.trim());
        if rest.is_empty() || rest == ":memory:" {
            None
        } else {
            Some(rest.to_string())
        }
    }

    /// Open the writer connection for the DSN. A file database is put in **WAL**
    /// journal mode (ADR 0017): a writer and the pooled readers then run
    /// concurrently — readers see the last committed snapshot while the writer
    /// writes, instead of every statement serialising on one connection. An
    /// in-memory database cannot use WAL (it has no file to journal), so it keeps
    /// the single-connection behaviour.
    fn open(dsn: &str) -> Result<rusqlite::Connection> {
        match dsn_path(dsn) {
            None => rusqlite::Connection::open_in_memory().map_err(|e| Error::Db(e.to_string())),
            Some(path) => {
                let conn =
                    rusqlite::Connection::open(&path).map_err(|e| Error::Db(e.to_string()))?;
                // WAL is a persistent property of the database file, so setting it
                // on the writer is enough — a reader that opens the same file later
                // is already in WAL mode. `busy_timeout` lets a brief lock
                // contention wait rather than failing with `SQLITE_BUSY`.
                conn.pragma_update(None, "journal_mode", "WAL")
                    .map_err(|e| Error::Db(e.to_string()))?;
                conn.busy_timeout(std::time::Duration::from_millis(5_000))
                    .map_err(|e| Error::Db(e.to_string()))?;
                Ok(conn)
            }
        }
    }

    /// Open a read-only connection to a file database for the reader pool. WAL is
    /// already set by the writer, so a reader only needs its own `busy_timeout`.
    fn open_reader(path: &str) -> Result<rusqlite::Connection> {
        use rusqlite::OpenFlags;
        let conn = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|e| Error::Db(e.to_string()))?;
        conn.busy_timeout(std::time::Duration::from_millis(5_000))
            .map_err(|e| Error::Db(e.to_string()))?;
        Ok(conn)
    }

    pub(super) async fn fetch(sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        // A standalone read on a file database goes to the reader pool — concurrent
        // with the writer under WAL. Inside a transaction, or on `:memory:`, it
        // stays on the one connection the transaction or the database holds, so it
        // still reads its own uncommitted writes.
        if !in_tx() {
            if let Some(pool) = readers() {
                return read_pooled(pool, sql, params).await;
            }
        }
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

    /// Run a row-returning statement on one connection — the shape the reader pool
    /// and the shared-connection path both need. Synchronous: the callers wrap it
    /// in [`blocking`].
    fn query_on(
        conn: &rusqlite::Connection,
        sql: &str,
        params: Vec<rusqlite::types::Value>,
    ) -> Result<Vec<Row>> {
        let mut stmt = conn.prepare(sql).map_err(db_err)?;
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
    }

    /// Run a row-returning statement on the **shared** connection, assuming the
    /// caller holds the connection lock (standalone statement) or is inside a
    /// transaction.
    async fn fetch_on_conn(sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let conn = conn()?;
        let sql = sql.to_string();
        let params: Vec<rusqlite::types::Value> = params.iter().map(value_to_sqlite).collect();
        blocking(move || {
            let guard = conn.lock().map_err(|_| poisoned())?;
            query_on(&guard, &sql, params)
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

    /// A SQLite transaction held open across calls for the interactive shell —
    /// the SQLite side of [`super::InteractiveTransaction`].
    ///
    /// It owns [`TX_LOCK`] for the whole span (the guard lives in `_guard`), so
    /// no other task can observe the single connection mid-transaction; and it
    /// carries the [`SQLITE_TX`] state that [`scope_interactive`] re-enters for
    /// each line, which is what tells the ordinary `fetch`/`run` they are already
    /// inside a transaction and must not re-acquire the lock.
    pub(super) struct InteractiveTx {
        _guard: tokio::sync::OwnedMutexGuard<()>,
        state: SqliteTxState,
        done: std::cell::Cell<bool>,
    }

    impl Drop for InteractiveTx {
        fn drop(&mut self) {
            if self.done.get() {
                return;
            }
            // Committed neither way: roll back, best-effort and synchronously —
            // the same move [`RollbackOnPanic`] makes. Nothing an uncommitted
            // interactive transaction wrote survives the handle being dropped.
            if let Ok(c) = conn() {
                if let Ok(guard) = c.lock() {
                    let _ = guard.execute_batch("ROLLBACK");
                }
            }
        }
    }

    pub(super) async fn begin_interactive() -> Result<InteractiveTx> {
        let guard = tx_lock().lock_owned().await;
        control("BEGIN").await?;
        Ok(InteractiveTx {
            _guard: guard,
            state: SqliteTxState {
                depth: Arc::new(Mutex::new(1)),
            },
            done: std::cell::Cell::new(false),
        })
    }

    pub(super) async fn commit_interactive(tx: &InteractiveTx) -> Result<()> {
        control("COMMIT").await?;
        tx.done.set(true);
        Ok(())
    }

    pub(super) async fn rollback_interactive(tx: &InteractiveTx) -> Result<()> {
        control("ROLLBACK").await?;
        tx.done.set(true);
        Ok(())
    }

    /// Run `fut` with this transaction's [`SQLITE_TX`] state in scope, so the
    /// statements inside it run on the connection the transaction already holds
    /// rather than re-acquiring [`TX_LOCK`].
    pub(super) async fn scope_interactive<Fut, T>(tx: &InteractiveTx, fut: Fut) -> T
    where
        Fut: std::future::Future<Output = T>,
    {
        SQLITE_TX.scope(tx.state.clone(), fut).await
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

/// The MySQL backend: its own `mysql_async` pool (ADR 0018), a sibling of the
/// `sqlite` module. Like SQLite it reads the DSN from the process environment and
/// does not use the `Database` handle — session/replica wait for the Database↔
/// Driver refactor. The non-scalar types travel as text/native values via
/// [`value_to_mysql`]/the `mysql_*` decoders in [`crate::row`].
///
/// Every connection checkout runs the `setup` statements: **`ANSI_QUOTES`** so the
/// compiler's double-quoted identifiers are identifiers, not string literals;
/// **`time_zone='+00:00'`** so a `DATETIME` round-trips a `DateTime<Utc>`; and
/// **`utf8mb4`**. They are `setup` rather than `init` so they are re-applied after
/// the pool resets a reused connection.
#[cfg(feature = "mysql")]
mod mysql {
    use std::sync::{Arc, OnceLock};

    use mysql_async::prelude::Queryable;

    use super::{dsn, savepoint_name, Error, Result, Tx};
    use crate::compiler::positionalize;
    use crate::row::{value_to_mysql, Row};
    use crate::value::Value;

    static MYSQL_POOL: OnceLock<mysql_async::Pool> = OnceLock::new();

    /// The connection a MySQL transaction runs on, plus its savepoint depth —
    /// the mirror of PostgreSQL's `TxState`, with a `mysql_async::Conn` in place of
    /// the pooled `Object`.
    struct MySqlTxState {
        conn: Option<mysql_async::Conn>,
        depth: usize,
    }

    type Shared = Arc<tokio::sync::Mutex<MySqlTxState>>;

    tokio::task_local! {
        /// The transaction in scope, if any. Set by [`atomic`] for the closure's
        /// duration, so a nested `fetch`/`run` finds it and runs on its connection.
        static MYSQL_TX: Shared;
    }

    /// The pool, built once from the DSN with the per-connection `init` fixups.
    fn pool() -> Result<&'static mysql_async::Pool> {
        if let Some(p) = MYSQL_POOL.get() {
            return Ok(p);
        }
        let url = dsn()?;
        let opts = mysql_async::Opts::from_url(&url)
            .map_err(|e| Error::Config(format!("the MySQL DSN is not usable: {e}")))?;
        let builder = mysql_async::OptsBuilder::from_opts(opts).setup(vec![
            // `setup`, not `init`: these run on every checkout, so they survive the
            // pool's `COM_RESET_CONNECTION` on return. `init` runs only once at
            // connect, and the reset then wipes `sql_mode` — a reused connection
            // would lose `ANSI_QUOTES` and reject the compiler's `"ident"` quoting.
            // Append the mode, never overwrite, so the server's strict mode is kept.
            "SET SESSION sql_mode=CONCAT(@@sql_mode, ',ANSI_QUOTES')".to_string(),
            "SET time_zone='+00:00'".to_string(),
            "SET NAMES utf8mb4".to_string(),
        ]);
        Ok(MYSQL_POOL.get_or_init(|| mysql_async::Pool::new(builder)))
    }

    /// Acquire a connection, retrying a transient failure with the same capped
    /// backoff as the PostgreSQL path (ADR 0015).
    async fn conn() -> Result<mysql_async::Conn> {
        let max = super::env_u64(super::RETRY_MAX_ENV, super::RETRY_MAX_DEFAULT);
        let base = super::env_u64(super::RETRY_BASE_MS_ENV, super::RETRY_BASE_MS_DEFAULT);
        let mut attempt: u32 = 0;
        loop {
            match pool()?.get_conn().await {
                Ok(c) => return Ok(c),
                Err(e) => {
                    if u64::from(attempt) >= max {
                        return Err(Error::Pool(format!(
                            "could not acquire a MySQL connection: {e}"
                        )));
                    }
                    tokio::time::sleep(super::backoff_delay(base, attempt)).await;
                    attempt += 1;
                }
            }
        }
    }

    /// The numbered `?n` the compiler emits, converted to the bare positional `?`
    /// MySQL binds by order, with the parameters reordered to match (ADR 0018).
    fn positional(sql: &str, params: &[Value]) -> (String, Vec<mysql_async::Value>) {
        let (text, ordered) = positionalize(sql, params);
        (text, ordered.iter().map(value_to_mysql).collect())
    }

    pub(super) async fn fetch(sql: &str, params: &[Value]) -> Result<Vec<Row>> {
        let (text, params) = positional(sql, params);
        if let Ok(tx) = MYSQL_TX.try_with(|t| t.clone()) {
            let mut guard = tx.lock().await;
            let conn = guard.conn.as_mut().ok_or_else(tx_gone)?;
            let rows: Vec<mysql_async::Row> = conn.exec(text, params).await.map_err(db_err)?;
            return Ok(rows.into_iter().map(Row::MySql).collect());
        }
        let mut conn = conn().await?;
        let rows: Vec<mysql_async::Row> = conn.exec(text, params).await.map_err(db_err)?;
        Ok(rows.into_iter().map(Row::MySql).collect())
    }

    pub(super) async fn run(sql: &str, params: &[Value]) -> Result<u64> {
        let (text, params) = positional(sql, params);
        if let Ok(tx) = MYSQL_TX.try_with(|t| t.clone()) {
            let mut guard = tx.lock().await;
            let conn = guard.conn.as_mut().ok_or_else(tx_gone)?;
            conn.exec_drop(text, params).await.map_err(db_err)?;
            return Ok(conn.affected_rows());
        }
        let mut conn = conn().await?;
        conn.exec_drop(text, params).await.map_err(db_err)?;
        Ok(conn.affected_rows())
    }

    /// Run an `INSERT` and report `(rows affected, last_insert_id)` — the verb the
    /// no-`RETURNING` path uses to find the generated keys. `last_insert_id` is
    /// per-connection, so this must run inside the transaction that then reloads
    /// the rows. Wired by the create path in ADR 0018 Inc. 2b.
    pub(super) async fn insert(sql: &str, params: &[Value]) -> Result<(u64, u64)> {
        let (text, params) = positional(sql, params);
        if let Ok(tx) = MYSQL_TX.try_with(|t| t.clone()) {
            let mut guard = tx.lock().await;
            let conn = guard.conn.as_mut().ok_or_else(tx_gone)?;
            conn.exec_drop(text, params).await.map_err(db_err)?;
            return Ok((conn.affected_rows(), conn.last_insert_id().unwrap_or(0)));
        }
        let mut conn = conn().await?;
        conn.exec_drop(text, params).await.map_err(db_err)?;
        Ok((conn.affected_rows(), conn.last_insert_id().unwrap_or(0)))
    }

    pub(super) async fn run_ddl(ddl: &str) -> Result<()> {
        // DDL auto-commits in MySQL, so it is never run on a transaction's
        // connection; a standalone checkout is correct and avoids surprising a
        // transaction with an implicit commit.
        let mut conn = conn().await?;
        conn.query_drop(ddl).await.map_err(db_err)
    }

    /// Run `f` inside a transaction (ADR 0018), the MySQL mirror of `atomic_pg`:
    /// an owned `Conn` in a task-local, `BEGIN`/`SAVEPOINT` issued directly. InnoDB
    /// supports savepoints, so nesting is a savepoint exactly as on PostgreSQL.
    pub(super) async fn atomic<F, Fut, T>(f: F) -> Result<T>
    where
        F: FnOnce(Tx) -> Fut,
        Fut: std::future::Future<Output = Result<T>>,
    {
        match MYSQL_TX.try_with(|t| t.clone()) {
            // Already inside one: a savepoint.
            Ok(shared) => {
                let depth = {
                    let mut state = shared.lock().await;
                    state.depth += 1;
                    let depth = state.depth;
                    let conn = state.conn.as_mut().ok_or_else(tx_gone)?;
                    conn.query_drop(format!("SAVEPOINT {}", savepoint_name(depth)))
                        .await
                        .map_err(db_err)?;
                    depth
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
                let conn = state.conn.as_mut().ok_or_else(tx_gone)?;
                match outcome {
                    Ok(v) => {
                        conn.query_drop(format!("RELEASE SAVEPOINT {name}"))
                            .await
                            .map_err(db_err)?;
                        Ok(v)
                    }
                    Err(e) => {
                        conn.query_drop(format!("ROLLBACK TO SAVEPOINT {name}"))
                            .await
                            .map_err(db_err)?;
                        conn.query_drop(format!("RELEASE SAVEPOINT {name}"))
                            .await
                            .map_err(db_err)?;
                        Err(e)
                    }
                }
            }
            // The outermost transaction.
            Err(_) => {
                let mut conn = conn().await?;
                conn.query_drop("BEGIN").await.map_err(db_err)?;
                let shared: Shared = Arc::new(tokio::sync::Mutex::new(MySqlTxState {
                    conn: Some(conn),
                    depth: 1,
                }));
                let guard = PanicGuard {
                    shared: shared.clone(),
                    armed: true,
                };
                let outcome = MYSQL_TX.scope(shared.clone(), f(Tx { depth: 1 })).await;
                let mut guard = guard;
                guard.armed = false;

                let mut state = shared.lock().await;
                let mut conn = state.conn.take().ok_or_else(tx_gone)?;
                let result = match outcome {
                    Ok(v) => conn.query_drop("COMMIT").await.map(|()| v).map_err(db_err),
                    Err(e) => {
                        // The rollback's own failure must not hide the error that
                        // caused it; that is the one the caller needs to read.
                        let _ = conn.query_drop("ROLLBACK").await;
                        Err(e)
                    }
                };
                // Dropping the `Conn` returns it to the pool now that no transaction
                // is open.
                drop(conn);
                result
            }
        }
    }

    /// Closes the connection if dropped while still holding it — which only happens
    /// on a panic, and is what makes MySQL roll the transaction back (a connection
    /// closing mid-transaction is rolled back by the server), the same guarantee
    /// `PanicGuard` gives the PostgreSQL path.
    struct PanicGuard {
        shared: Shared,
        armed: bool,
    }

    impl Drop for PanicGuard {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            if let Ok(mut state) = self.shared.try_lock() {
                // Take the connection out so it is closed rather than returned to
                // the pool with an open transaction; the server rolls it back.
                let _ = state.conn.take();
            }
        }
    }

    fn tx_gone() -> Error {
        Error::Db("the MySQL transaction's connection is gone; it was rolled back".into())
    }

    fn db_err(e: mysql_async::Error) -> Error {
        // Record a server error's code in brackets, as `from_pg` records the
        // PostgreSQL SQLSTATE, so `is_unique_violation` can recognise a duplicate
        // key (`ER_DUP_ENTRY`, code 1062) without parsing mysql_async's `Display`.
        if let mysql_async::Error::Server(ref se) = e {
            return Error::Db(format!(
                "ERROR [{}] ({}): {}",
                se.code, se.state, se.message
            ));
        }
        Error::Db(e.to_string())
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
        #[cfg(feature = "mysql")]
        Backend::MySql => mysql::atomic(f).await,
        #[cfg(not(feature = "mysql"))]
        Backend::MySql => Err(mysql_disabled()),
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
            let conn = acquire(Route::Primary).await?;
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

// ------------------------------------------------- interactive transactions
//
// [`atomic`] brackets a *single* future: it begins, runs the closure, and ends
// before it returns, because the transaction lives in a task-local that is only
// in scope for that future. The shell cannot use it — each REPL line is its own
// `block_on`, so a transaction begun on one line would be gone by the next, the
// task-local dropped with the future that set it.
//
// So the shell gets a handle instead. [`begin_interactive`] takes the dedicated
// connection and issues `BEGIN`, handing back an [`InteractiveTransaction`] the
// caller keeps between lines; it wraps each following line in
// [`InteractiveTransaction::scope`], which puts that same connection back in the
// task-local so the *ordinary* routing (`query`/`execute`, and the SQLite
// `fetch`/`run`) finds it unchanged; `commit`/`rollback` end the transaction and
// return the connection. The statements in between are exactly the standalone
// ones — the handle only controls *when* the transaction opens and closes.

/// A transaction opened interactively and held open across calls — the
/// counterpart to [`atomic`] for a session that spans more than one future,
/// which is what a REPL's line-by-line `block_on` is.
///
/// Drop it without committing — on `:quit`, or a panic unwinding the shell — and
/// the transaction is rolled back: PostgreSQL's connection is closed rather than
/// recycled (an open transaction dying with its socket is rolled back by the
/// server), and SQLite issues a best-effort `ROLLBACK`. Nothing a session began
/// but did not commit is ever left written.
pub struct InteractiveTransaction(TxHandle);

enum TxHandle {
    /// A dedicated pooled connection with an open `BEGIN`, kept out of the pool
    /// for the transaction's life. The same `Shared` the task-local routing in
    /// [`query_inner`]/[`execute_inner`] reads.
    Postgres(Shared),
    #[cfg(feature = "sqlite")]
    Sqlite(sqlite::InteractiveTx),
}

/// Open a transaction and hand back a handle that stays open until the caller
/// commits or rolls back. See the section note for why the shell needs this
/// rather than [`atomic`].
///
/// The backend is the DSN's, mirroring [`atomic`]: PostgreSQL takes a dedicated
/// pooled connection and `BEGIN`s on it (`BEGIN READ ONLY` in a read-only
/// session, so the server enforces what [`guard_write`] names); SQLite holds its
/// single shared connection for the transaction's scope.
pub async fn begin_interactive() -> Result<InteractiveTransaction> {
    match backend()? {
        Backend::Postgres => {
            let conn = acquire(Route::Primary).await?;
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
            Ok(InteractiveTransaction(TxHandle::Postgres(shared)))
        }
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => Ok(InteractiveTransaction(TxHandle::Sqlite(
            sqlite::begin_interactive().await?,
        ))),
        #[cfg(not(feature = "sqlite"))]
        Backend::Sqlite => Err(sqlite_disabled()),
        Backend::MySql => Err(Error::Db(
            "MySQL interactive transactions are not implemented yet (ADR 0018 Inc. 2)".into(),
        )),
    }
}

impl InteractiveTransaction {
    /// Run `fut` with this transaction's connection in scope, so the ordinary
    /// statements inside it — a `fetch`, a `run`, a raw `sql` — route onto the
    /// transaction instead of a fresh pooled connection. The shell wraps every
    /// line it reads while a transaction is open in this.
    pub async fn scope<Fut, T>(&self, fut: Fut) -> T
    where
        Fut: std::future::Future<Output = T>,
    {
        match &self.0 {
            TxHandle::Postgres(shared) => CURRENT_TX.scope(shared.clone(), fut).await,
            #[cfg(feature = "sqlite")]
            TxHandle::Sqlite(tx) => sqlite::scope_interactive(tx, fut).await,
        }
    }

    /// Commit, persisting every statement run since `begin` atomically, and
    /// return the connection. Consumes the handle, so a committed transaction
    /// cannot be used again.
    pub async fn commit(self) -> Result<()> {
        match &self.0 {
            TxHandle::Postgres(shared) => {
                let conn = shared
                    .lock()
                    .await
                    .conn
                    .take()
                    .ok_or_else(|| Error::Db("the transaction's connection is gone".into()))?;
                match simple(&conn, "COMMIT").await {
                    // Clean after COMMIT — recycle it back into the pool.
                    Ok(()) => {
                        drop(conn);
                        Ok(())
                    }
                    // Uncertain after a failed COMMIT — close it rather than
                    // return a connection in an unknown state to the pool.
                    Err(e) => {
                        let _ = Object::take(conn);
                        Err(e)
                    }
                }
            }
            #[cfg(feature = "sqlite")]
            TxHandle::Sqlite(tx) => sqlite::commit_interactive(tx).await,
        }
        // `self` drops here: PostgreSQL's conn is already `None`, SQLite is
        // marked done — so the `Drop` rollback is a no-op either way.
    }

    /// Roll back, discarding every statement run since `begin`, and return the
    /// connection. Consumes the handle.
    pub async fn rollback(self) -> Result<()> {
        match &self.0 {
            TxHandle::Postgres(shared) => {
                let conn = shared
                    .lock()
                    .await
                    .conn
                    .take()
                    .ok_or_else(|| Error::Db("the transaction's connection is gone".into()))?;
                match simple(&conn, "ROLLBACK").await {
                    Ok(()) => {
                        drop(conn);
                        Ok(())
                    }
                    Err(e) => {
                        let _ = Object::take(conn);
                        Err(e)
                    }
                }
            }
            #[cfg(feature = "sqlite")]
            TxHandle::Sqlite(tx) => sqlite::rollback_interactive(tx).await,
        }
    }

    /// Which backend this transaction is running on.
    pub fn backend(&self) -> Backend {
        match &self.0 {
            TxHandle::Postgres(_) => Backend::Postgres,
            #[cfg(feature = "sqlite")]
            TxHandle::Sqlite(_) => Backend::Sqlite,
        }
    }
}

impl Drop for InteractiveTransaction {
    fn drop(&mut self) {
        match &self.0 {
            // A PostgreSQL transaction abandoned without commit/rollback: take
            // the connection out of the pool so it is closed, not recycled with
            // an open transaction — the server rolls it back when the socket
            // closes. The same guarantee [`PanicGuard`] gives `atomic`. Already
            // `None` after `commit`/`rollback`, so those paths do nothing here.
            TxHandle::Postgres(shared) => {
                if let Ok(mut state) = shared.try_lock() {
                    if let Some(conn) = state.conn.take() {
                        let _ = Object::take(conn);
                    }
                }
            }
            // SQLite's own handle drops right after this and rolls back there if
            // it was not committed — see [`sqlite::InteractiveTx`].
            #[cfg(feature = "sqlite")]
            TxHandle::Sqlite(_) => {}
        }
    }
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

    #[tokio::test]
    async fn set_local_refuses_a_guc_outside_the_allow_list_before_touching_the_database() {
        // The key of a `SET` is spliced, not bound, so a key off the allow-list
        // must be refused up front — this returns before any connection is used.
        let e = set_local("app.evil; DROP TABLE x", "1")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a settable GUC"), "{e}");
    }

    #[tokio::test]
    async fn set_local_refuses_an_allowed_guc_outside_a_transaction() {
        // A `SET LOCAL` outside a transaction is a silent no-op; refuse it so the
        // caller is not misled into thinking the GUC took.
        let e = set_local("dorm.tenant", "acme")
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("inside a transaction"), "{e}");
    }

    #[test]
    fn the_rls_tenant_guc_is_on_the_set_local_allow_list() {
        // ADR 0021's RLS reads `dorm.tenant`; if it left the list the policies
        // would silently stop isolating.
        assert!(SET_LOCAL_ALLOWED.contains(&"dorm.tenant"));
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
    fn sslmode_maps_to_a_requirement_or_a_named_refusal() {
        assert_eq!(ssl_requirement(None).unwrap(), SslRequirement::Disable);
        assert_eq!(
            ssl_requirement(Some("disable")).unwrap(),
            SslRequirement::Disable
        );
        // The opportunistic modes are Disable: DORM does not half-encrypt.
        assert_eq!(
            ssl_requirement(Some("prefer")).unwrap(),
            SslRequirement::Disable
        );
        assert_eq!(
            ssl_requirement(Some("require")).unwrap(),
            SslRequirement::Require
        );
        assert_eq!(
            ssl_requirement(Some(" Verify-Full ")).unwrap(),
            SslRequirement::VerifyFull
        );
        // verify-ca is a documented gap, and it says so.
        let e = ssl_requirement(Some("verify-ca")).unwrap_err().to_string();
        assert!(e.contains("verify-ca"), "{e}");
        assert!(ssl_requirement(Some("banana")).is_err());
    }

    #[test]
    fn tls_params_are_pulled_out_of_the_keyword_dsn_leaving_the_rest() {
        let (cleaned, mode, root) = extract_tls_params(
            "host=/var/run/postgresql user=me sslmode=verify-full sslrootcert=/etc/ca.pem dbname=x",
        );
        assert_eq!(cleaned, "host=/var/run/postgresql user=me dbname=x");
        assert_eq!(mode.as_deref(), Some("verify-full"));
        assert_eq!(root.as_deref(), Some("/etc/ca.pem"));
    }

    #[test]
    fn tls_params_are_pulled_out_of_the_url_dsn_leaving_the_rest() {
        let (cleaned, mode, root) =
            extract_tls_params("postgres://me@db:5432/app?sslmode=require&application_name=dorm");
        assert_eq!(cleaned, "postgres://me@db:5432/app?application_name=dorm");
        assert_eq!(mode.as_deref(), Some("require"));
        assert_eq!(root, None);
    }

    #[test]
    fn a_dsn_without_tls_keys_is_unchanged() {
        let (cleaned, mode, root) = extract_tls_params("host=db user=me dbname=x");
        assert_eq!(cleaned, "host=db user=me dbname=x");
        assert_eq!(mode, None);
        assert_eq!(root, None);
    }

    #[test]
    fn statement_timeout_is_merged_into_options_and_zero_is_off() {
        assert_eq!(merge_statement_timeout(None, 0), None);
        assert_eq!(
            merge_statement_timeout(None, 5000).as_deref(),
            Some("-c statement_timeout=5000")
        );
        // An existing options string is preserved, not clobbered.
        assert_eq!(
            merge_statement_timeout(Some("-c search_path=app"), 5000).as_deref(),
            Some("-c search_path=app -c statement_timeout=5000")
        );
        // Zero leaves existing options alone.
        assert_eq!(
            merge_statement_timeout(Some("-c search_path=app"), 0).as_deref(),
            Some("-c search_path=app")
        );
    }

    #[test]
    fn backoff_doubles_each_attempt_and_is_capped() {
        // Attempt 0 waits the base, then it doubles: 50, 100, 200, 400…
        assert_eq!(backoff_delay(50, 0), Duration::from_millis(50));
        assert_eq!(backoff_delay(50, 1), Duration::from_millis(100));
        assert_eq!(backoff_delay(50, 2), Duration::from_millis(200));
        assert_eq!(backoff_delay(50, 3), Duration::from_millis(400));
        // The cap holds, and a huge attempt cannot overflow into a tiny wait.
        assert_eq!(
            backoff_delay(1000, 10),
            Duration::from_millis(RETRY_BACKOFF_CAP_MS)
        );
        assert_eq!(
            backoff_delay(1, 200),
            Duration::from_millis(RETRY_BACKOFF_CAP_MS)
        );
    }

    #[test]
    fn recycling_defaults_to_verified_and_honours_the_override() {
        let before = std::env::var(POOL_RECYCLING_ENV).ok();
        std::env::remove_var(POOL_RECYCLING_ENV);
        assert!(matches!(recycling_method(), RecyclingMethod::Verified));
        std::env::set_var(POOL_RECYCLING_ENV, "fast");
        assert!(matches!(recycling_method(), RecyclingMethod::Fast));
        std::env::set_var(POOL_RECYCLING_ENV, " Clean ");
        assert!(matches!(recycling_method(), RecyclingMethod::Clean));
        // Anything unrecognised falls back to the resilient default.
        std::env::set_var(POOL_RECYCLING_ENV, "banana");
        assert!(matches!(recycling_method(), RecyclingMethod::Verified));
        match before {
            Some(v) => std::env::set_var(POOL_RECYCLING_ENV, v),
            None => std::env::remove_var(POOL_RECYCLING_ENV),
        }
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
