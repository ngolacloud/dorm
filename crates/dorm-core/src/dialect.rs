//! `SqlDialect` — the single place a backend declares how its SQL diverges.
//!
//! Before this, the per-backend choices lived as `match` arms spread across each
//! method of the [`Dialect`](crate::compiler::Dialect) enum here AND as a *second*
//! `Backend` enum in `dorm-migrations` (`ddl.rs`), so adding an engine meant
//! editing ~8 `match` arms across two crates that could silently diverge (G1/E-A-1
//! of the Rust engineering audit, ADR 0011).
//!
//! The behaviour now lives in **one trait with one impl per backend**: a new
//! engine (MySQL, CockroachDB, MSSQL, Oracle) is a new `impl SqlDialect` plus one
//! selector arm, not an arm in every method. [`Dialect`](crate::compiler::Dialect)
//! stays as the public, `Copy` selector that maps to an `impl SqlDialect`
//! ([`Dialect::ops`](crate::compiler::Dialect::ops)); `dorm-migrations` can adopt
//! the SAME trait so the two dialect decisions stop being two sources of truth.
//!
//! The impls are zero-sized, so selecting one is `&'static` with no allocation.

use crate::compiler::Capability;

/// Everything a SQL backend decides differently. One impl per engine (ADR 0011).
///
/// Kept deliberately small: identifiers, joins and clause structure are shared
/// across the SQL backends (ADR 0002); only the items here diverge.
pub trait SqlDialect {
    /// A short, stable name for diagnostics (`"postgres"`, `"sqlite"`, `"mysql"`).
    fn name(&self) -> &'static str;

    /// The case-insensitive pattern operator: PostgreSQL's `ILIKE`, or `LIKE`
    /// where the engine's default collation already folds case (SQLite, MySQL).
    fn ilike(&self) -> &'static str;

    /// The character that introduces a bound-parameter placeholder: `$` on
    /// PostgreSQL (`$1`), `?` on SQLite/MySQL (`?1`).
    fn placeholder_marker(&self) -> char;

    /// A bound-parameter placeholder for binding position `n` (1-based). Numbered
    /// on every dialect (MySQL's driver later positionalises `?n` → bare `?`), so
    /// a `HAVING`-before-`WHERE` bind order stays unambiguous during compilation.
    fn placeholder(&self, n: usize) -> String {
        format!("{}{n}", self.placeholder_marker())
    }

    /// Whether the engine binds a bare positional `?` by order rather than a
    /// numbered placeholder (true only for MySQL; its driver runs `positionalize`).
    fn positional_unnumbered(&self) -> bool;

    /// The SQL expression for "now", used to refresh `auto_now` columns on write.
    fn now(&self) -> &'static str;

    /// Whether this backend has capability `cap` — the single source of truth the
    /// core consults before emitting a feature not every engine has (ADR 0011).
    fn supports(&self, cap: Capability) -> bool;

    /// Quote an identifier for this dialect: `"ident"` on PostgreSQL/SQLite, and
    /// `` `ident` `` on MySQL. The caller is responsible for only passing trusted,
    /// `&'static`-derived identifiers (the query compiler refuses any that embed a
    /// quote); this is the dialectal *formatting*, ready for `dorm-migrations`'
    /// DDL and the MySQL backend to adopt so a reserved word (`user`, `order`)
    /// survives (E-A-3).
    fn quote_ident(&self, ident: &str) -> String;
}

/// PostgreSQL (and the PG-wire CockroachDB) — the default, full-capability engine.
pub struct Postgres;
/// SQLite — embedded; no `CONCURRENTLY`/native boolean/`ILIKE`, but RETURNING and
/// transactional DDL since 3.35.
pub struct Sqlite;
/// MySQL — the dialect *profile* (ADR 0011 Phase 2.2); the driver is forthcoming.
pub struct MySql;

impl SqlDialect for Postgres {
    fn name(&self) -> &'static str {
        "postgres"
    }
    fn ilike(&self) -> &'static str {
        "ILIKE"
    }
    fn placeholder_marker(&self) -> char {
        '$'
    }
    fn positional_unnumbered(&self) -> bool {
        false
    }
    fn now(&self) -> &'static str {
        "now()"
    }
    fn supports(&self, _cap: Capability) -> bool {
        true
    }
    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{ident}\"")
    }
}

impl SqlDialect for Sqlite {
    fn name(&self) -> &'static str {
        "sqlite"
    }
    fn ilike(&self) -> &'static str {
        // SQLite's `LIKE` is case-insensitive for ASCII under the default collation.
        "LIKE"
    }
    fn placeholder_marker(&self) -> char {
        '?'
    }
    fn positional_unnumbered(&self) -> bool {
        false
    }
    fn now(&self) -> &'static str {
        "CURRENT_TIMESTAMP"
    }
    fn supports(&self, cap: Capability) -> bool {
        use Capability::*;
        match cap {
            Returning | Savepoints | DdlInTransaction => true,
            ConcurrentIndex | NativeBoolean | IlikeOperator => false,
        }
    }
    fn quote_ident(&self, ident: &str) -> String {
        format!("\"{ident}\"")
    }
}

impl SqlDialect for MySql {
    fn name(&self) -> &'static str {
        "mysql"
    }
    fn ilike(&self) -> &'static str {
        "LIKE"
    }
    fn placeholder_marker(&self) -> char {
        '?'
    }
    fn positional_unnumbered(&self) -> bool {
        true
    }
    fn now(&self) -> &'static str {
        "now()"
    }
    fn supports(&self, cap: Capability) -> bool {
        use Capability::*;
        match cap {
            Savepoints => true,
            Returning | DdlInTransaction | ConcurrentIndex | NativeBoolean | IlikeOperator => false,
        }
    }
    fn quote_ident(&self, ident: &str) -> String {
        format!("`{ident}`")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler::Capability;

    /// A per-dialect conformance check: the divergent choices match each engine's
    /// SQL, in ONE place. A new backend adds an `impl` + a row here.
    #[test]
    fn each_dialect_declares_its_divergent_sql() {
        // Identifier quoting (E-A-3): `"` on PG/SQLite, backtick on MySQL.
        assert_eq!(Postgres.quote_ident("user"), "\"user\"");
        assert_eq!(Sqlite.quote_ident("order"), "\"order\"");
        assert_eq!(MySql.quote_ident("user"), "`user`");

        // Placeholders: `$1` on PG, `?1` on SQLite/MySQL.
        assert_eq!(Postgres.placeholder(1), "$1");
        assert_eq!(Sqlite.placeholder(2), "?2");
        assert_eq!(MySql.placeholder(3), "?3");
        assert!(MySql.positional_unnumbered());
        assert!(!Postgres.positional_unnumbered());

        // Case-insensitive LIKE and `now()`.
        assert_eq!(Postgres.ilike(), "ILIKE");
        assert_eq!(Sqlite.ilike(), "LIKE");
        assert_eq!(Sqlite.now(), "CURRENT_TIMESTAMP");
        assert_eq!(MySql.now(), "now()");

        // Capability matrix: PG has all; SQLite lacks CONCURRENTLY; MySQL lacks RETURNING.
        assert!(Postgres.supports(Capability::ConcurrentIndex));
        assert!(!Sqlite.supports(Capability::ConcurrentIndex));
        assert!(Sqlite.supports(Capability::Returning));
        assert!(!MySql.supports(Capability::Returning));
        assert!(MySql.supports(Capability::Savepoints));
    }
}
