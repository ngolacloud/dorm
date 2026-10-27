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
            Error::Db(msg)
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

/// The SQLSTATE for `unique_violation`, which `get_or_create` has to tell apart
/// from a genuine failure when it loses a race.
pub const UNIQUE_VIOLATION: &str = "23505";

/// Whether this error is a unique-constraint violation.
pub fn is_unique_violation(e: &Error) -> bool {
    match e {
        Error::Db(msg) => msg.contains(UNIQUE_VIOLATION),
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
    fn other_database_errors_are_not_unique_violations() {
        assert!(!is_unique_violation(&Error::Db(
            "ERROR [42P01]: relation does not exist".into()
        )));
        assert!(!is_unique_violation(&Error::Pool("timed out".into())));
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
