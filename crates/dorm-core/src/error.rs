//! What the ORM can refuse, and why.
//!
//! The split that matters here is between errors the *query compiler* raises and
//! errors the *database* raises. The compiler's errors all name a model, a field
//! or a lookup, because the compiler's whole job is to reject a name that is not
//! in the registry before it ever reaches SQL. That is the guarantee the REPL
//! leans on: a model name typed by a human is validated against metadata, never
//! interpolated into a statement.

use thiserror::Error;

/// Every way an ORM call can fail.
#[derive(Debug, Error)]
pub enum Error {
    /// No model with that name was registered. Carries the name as typed so the
    /// REPL can say "did you mean" without a second lookup.
    #[error("unknown model `{0}`")]
    UnknownModel(String),

    #[error("model `{model}` has no field `{field}`")]
    UnknownField { model: String, field: String },

    /// A lookup path kept going after a field that is not a foreign key, e.g.
    /// `name__slug` where `name` is text.
    #[error("`{path}`: `{segment}` is not a relation, so the path cannot continue through it")]
    NotARelation { path: String, segment: String },

    /// A path ended on a foreign key with no lookup, e.g. `tenant__`.
    #[error("`{path}` ends on a relation; name a field of `{model}` or use `{path}` with `exact`")]
    DanglingRelation { path: String, model: String },

    #[error("unknown lookup `{lookup}` (on `{path}`)")]
    UnknownLookup { lookup: String, path: String },

    /// `range` wants two bounds, `in` wants at least one, the rest want exactly one.
    #[error("lookup `{lookup}` on `{path}` takes {want} value(s), got {got}")]
    LookupArity {
        lookup: &'static str,
        path: String,
        want: &'static str,
        got: usize,
    },

    /// `get()` found nothing. Django raises `DoesNotExist`; the name is kept.
    #[error("{model} matching the query does not exist")]
    DoesNotExist { model: String },

    /// `get()` found more than one. Reports the count, because "2" and "4000"
    /// call for very different reactions from whoever is holding the REPL.
    #[error("get() returned {count} {model} rows, where exactly one was expected")]
    MultipleObjectsReturned { model: String, count: usize },

    /// A column came back as a type the field metadata did not predict.
    #[error("column `{column}` is not a {want}")]
    UnexpectedColumnType { column: String, want: &'static str },

    /// The database said no. Already unpacked into something readable — see
    /// [`from_pg`], and the note there about why `Display` alone is useless.
    #[error("database: {0}")]
    Db(String),

    #[error("connection pool: {0}")]
    Pool(String),

    /// Missing or unusable configuration, e.g. no DSN in the environment.
    #[error("{0}")]
    Config(String),

    /// An `atomic` block asked to be rolled back. Carries whatever the caller
    /// threw, so a REPL `throw "rollback"` surfaces that word and not a generic
    /// transaction failure.
    #[error("rolled back: {0}")]
    RolledBack(String),

    /// A write was attempted in a session opened `--read-only`.
    #[error("this session is read-only; `{0}` would write")]
    ReadOnly(&'static str),

    /// The server cancelled a statement that ran past
    /// `DELONIX_ORM_STATEMENT_TIMEOUT_MS` (SQLSTATE `57014`). Named separately
    /// from [`Error::Db`] so a caller can retry or report it as a timeout rather
    /// than reading a generic "canceling statement" driver string (ADR 0004).
    #[error("statement timed out: {0}")]
    Timeout(String),

    /// The DSN asks for TLS, but this build has no TLS backend compiled in, or
    /// asks for an `sslmode` this release does not yet implement. Never a silent
    /// downgrade to plaintext (ADR 0004).
    #[error("{0}")]
    Tls(String),
}

/// The ORM's result type.
pub type Result<T> = std::result::Result<T, Error>;

/// Turn a `tokio_postgres::Error` into something a human can act on.
///
/// This exists because `tokio_postgres::Error`'s `Display` is the bare string
/// `"db error"` — the useful part (SQLSTATE, the server's message, the failing
/// constraint) hangs off `as_db_error()`. The same lesson is already written down
/// in this repo at `crates/delonix-api/src/cpstore/store_tenants.rs:454`; losing
/// it here would make every constraint violation in the REPL indistinguishable.
pub fn from_pg(e: tokio_postgres::Error) -> Error {
    match e.as_db_error() {
        Some(db) => {
            let mut msg = format!("{} [{}]: {}", db.severity(), db.code().code(), db.message());
            if let Some(detail) = db.detail() {
                msg.push_str(" — ");
                msg.push_str(detail);
            }
            if let Some(constraint) = db.constraint() {
                msg.push_str(&format!(" (constraint `{constraint}`)"));
            }
            classify_db(db.code().code(), msg)
        }
        // No server error attached: a connection that died, a TLS failure, a
        // type mismatch caught client-side. `source()` carries the detail.
        None => {
            let mut msg = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                msg.push_str(": ");
                msg.push_str(&s.to_string());
                src = s.source();
            }
            Error::Db(msg)
        }
    }
}

/// Decide which [`enum@Error`] a server error becomes from its SQLSTATE and its
/// already-formatted message. Factored out of [`from_pg`] so the code→variant
/// mapping is testable without constructing a `tokio_postgres::DbError`, which
/// has no public constructor.
pub fn classify_db(code: &str, msg: String) -> Error {
    match code {
        // The server cancelled the statement — almost always our
        // `statement_timeout` (ADR 0004). Surface it as a timeout, not a
        // generic database error, so a caller can react to it as one.
        QUERY_CANCELED => Error::Timeout(msg),
        _ => Error::Db(msg),
    }
}

/// The SQLSTATE for `unique_violation`, which `get_or_create` has to tell apart
/// from a genuine failure when it loses a race.
pub const UNIQUE_VIOLATION: &str = "23505";

/// MySQL/MariaDB's `ER_DUP_ENTRY` error code. Unlike PostgreSQL there is no
/// dedicated SQLSTATE for a duplicate key (it is the generic `23000`), so
/// `get_or_create` keys off the error code, which the MySQL driver records in
/// brackets just as [`from_pg`] records the PostgreSQL SQLSTATE (ADR 0018).
pub const MYSQL_DUPLICATE_ENTRY: &str = "1062";

/// The SQLSTATE for `query_canceled` — what `statement_timeout` raises when it
/// fires. [`classify_db`] turns it into [`Error::Timeout`].
pub const QUERY_CANCELED: &str = "57014";

/// Whether this error is a unique-constraint violation, on any backend.
pub fn is_unique_violation(e: &Error) -> bool {
    match e {
        Error::Db(msg) => {
            msg.contains(&format!("[{UNIQUE_VIOLATION}]"))
                || msg.contains(&format!("[{MYSQL_DUPLICATE_ENTRY}]"))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_violation_is_recognised_from_the_formatted_message() {
        // `from_pg` writes the SQLSTATE in brackets; this is the coupling that
        // `get_or_create` depends on, so it gets a test of its own.
        let e = Error::Db(format!("ERROR [{UNIQUE_VIOLATION}]: duplicate key value"));
        assert!(is_unique_violation(&e));
    }

    #[test]
    fn a_mysql_duplicate_key_is_a_unique_violation() {
        // The MySQL driver's `db_err` records the error code in brackets (code
        // 1062, SQLSTATE 23000), which is what `get_or_create` keys off on MySQL.
        let e = Error::Db(format!(
            "ERROR [{MYSQL_DUPLICATE_ENTRY}] (23000): Duplicate entry 'x' for key 'name'"
        ));
        assert!(is_unique_violation(&e));
    }

    #[test]
    fn other_database_errors_are_not_unique_violations() {
        assert!(!is_unique_violation(&Error::Db(
            "ERROR [42P01]: relation does not exist".into()
        )));
        // A MySQL error that is not a duplicate key (e.g. a lock-wait timeout).
        assert!(!is_unique_violation(&Error::Db(
            "ERROR [1205] (HY000): Lock wait timeout exceeded".into()
        )));
        assert!(!is_unique_violation(&Error::Pool("timed out".into())));
    }

    #[test]
    fn a_cancelled_statement_is_classified_as_a_timeout_not_a_generic_db_error() {
        // This is the mapping ADR 0004 promises: `statement_timeout` fires,
        // PostgreSQL raises SQLSTATE 57014, and the caller reads a named timeout.
        let msg = "ERROR [57014]: canceling statement due to statement timeout".to_string();
        match classify_db(QUERY_CANCELED, msg.clone()) {
            Error::Timeout(m) => assert_eq!(m, msg),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn an_ordinary_server_error_stays_a_db_error() {
        match classify_db("42P01", "ERROR [42P01]: relation does not exist".into()) {
            Error::Db(_) => {}
            other => panic!("expected Db, got {other:?}"),
        }
    }

    #[test]
    fn messages_name_the_thing_that_was_wrong() {
        let e = Error::UnknownField {
            model: "Tenant".into(),
            field: "slog".into(),
        };
        assert_eq!(e.to_string(), "model `Tenant` has no field `slog`");

        let e = Error::MultipleObjectsReturned {
            model: "Tenant".into(),
            count: 3,
        };
        assert!(e.to_string().contains('3'));
    }
}
