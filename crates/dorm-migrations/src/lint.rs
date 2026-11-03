//! `dorm migrate lint`: flag operations that are unsafe under a rolling upgrade
//! (ADR 0022), so the dangerous migration fails CI instead of production.
//!
//! The analysis is deliberately a string scan over the generated DDL: the
//! statements come from the migrator in a predictable shape, and a lint that
//! parses SQL fully would be a second compiler to keep in step. It is a tripwire,
//! not a proof — a `Dangerous` finding is a "stop and look", not an impossibility.

use crate::migrator::Migration;

/// How bad a finding is. `Dangerous` fails the lint (non-zero exit in CI);
/// `Warning` is surfaced but does not fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Dangerous,
    Warning,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Dangerous => "DANGEROUS",
            Severity::Warning => "warning",
        }
    }
}

/// One thing the lint noticed, pinned to the migration and statement it is in.
#[derive(Debug, Clone)]
pub struct Finding {
    pub version: i64,
    pub statement: String,
    pub severity: Severity,
    pub message: &'static str,
}

/// Normalise a statement for scanning: upper-cased, whitespace collapsed.
fn normalise(stmt: &str) -> String {
    stmt.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase()
}

/// Lint one statement, returning the findings it triggers (usually none).
fn lint_statement(n: &str) -> Vec<(Severity, &'static str)> {
    let mut out = Vec::new();
    let has = |needle: &str| n.contains(needle);

    if has("DROP TABLE") {
        out.push((
            Severity::Dangerous,
            "drops a table — data loss; drop only after version N-1 has stopped using it",
        ));
    }
    if has("DROP COLUMN") {
        out.push((
            Severity::Dangerous,
            "drops a column — data loss; if this is a rename, model it as a rename, and drop \
             only after N-1 no longer reads it",
        ));
    }
    // A NOT NULL column with no default is rejected outright on a populated table,
    // and takes a long lock even when accepted.
    if has("ADD COLUMN") && has("NOT NULL") && !has("DEFAULT") {
        out.push((
            Severity::Dangerous,
            "adds a NOT NULL column without a default — rejected on a populated table; add it \
             nullable, backfill in batches, then add the NOT NULL constraint as NOT VALID + \
             VALIDATE",
        ));
    }
    // A type change rewrites the table under an ACCESS EXCLUSIVE lock.
    if has("ALTER COLUMN") && has(" TYPE ") {
        out.push((
            Severity::Dangerous,
            "changes a column's type — rewrites the whole table under an exclusive lock",
        ));
    }
    // A non-concurrent index holds a lock for the whole build.
    if has("CREATE INDEX") && !has("CONCURRENTLY") {
        out.push((
            Severity::Warning,
            "a non-concurrent CREATE INDEX holds a write lock for the whole build — prefer \
             CREATE INDEX CONCURRENTLY (outside a transaction)",
        ));
    }
    // A rename is not backward-compatible with the still-running N-1.
    if has(" RENAME ") {
        out.push((
            Severity::Warning,
            "a rename is not backward-compatible with the running previous version — do it in \
             two releases (add new, backfill, switch, drop old)",
        ));
    }
    out
}

/// Lint a set of migrations, returning every finding across their forward
/// statements, in migration-then-statement order.
pub fn lint(migrations: &[Migration]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for m in migrations {
        for stmt in &m.statements {
            for (severity, message) in lint_statement(&normalise(stmt)) {
                findings.push(Finding {
                    version: m.version,
                    statement: stmt.clone(),
                    severity,
                    message,
                });
            }
        }
    }
    findings
}

/// Whether any finding fails the lint (a `Dangerous` one).
pub fn has_blocking(findings: &[Finding]) -> bool {
    findings.iter().any(|f| f.severity == Severity::Dangerous)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mig(stmts: &[&str]) -> Migration {
        Migration {
            version: 1,
            name: "t".into(),
            statements: stmts.iter().map(|s| s.to_string()).collect(),
            down: Vec::new(),
        }
    }

    #[test]
    fn a_safe_additive_migration_has_no_findings() {
        let m = mig(&[
            "CREATE TABLE widgets (id BIGINT PRIMARY KEY, name TEXT NOT NULL)",
            "ALTER TABLE widgets ADD COLUMN note TEXT",
        ]);
        assert!(lint(&[m]).is_empty());
    }

    #[test]
    fn a_dropped_column_is_dangerous_and_blocks() {
        let f = lint(&[mig(&["ALTER TABLE widgets DROP COLUMN note"])]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Dangerous);
        assert!(has_blocking(&f));
    }

    #[test]
    fn a_not_null_add_without_default_is_dangerous_but_with_default_is_fine() {
        let bad = lint(&[mig(&["ALTER TABLE widgets ADD COLUMN qty INT NOT NULL"])]);
        assert!(has_blocking(&bad), "NOT NULL without default must block");

        let ok = lint(&[mig(&[
            "ALTER TABLE widgets ADD COLUMN qty INT NOT NULL DEFAULT 0",
        ])]);
        assert!(ok.is_empty(), "a default makes it safe");
    }

    #[test]
    fn a_type_change_is_dangerous() {
        let f = lint(&[mig(&["ALTER TABLE widgets ALTER COLUMN qty TYPE BIGINT"])]);
        assert!(has_blocking(&f));
    }

    #[test]
    fn a_non_concurrent_index_warns_but_does_not_block() {
        let f = lint(&[mig(&["CREATE INDEX widgets_name ON widgets (name)"])]);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Warning);
        assert!(!has_blocking(&f));

        // The concurrent form is clean.
        assert!(lint(&[mig(&[
            "CREATE INDEX CONCURRENTLY widgets_name ON widgets (name)"
        ])])
        .is_empty());
    }

    #[test]
    fn a_rename_warns() {
        let f = lint(&[mig(&["ALTER TABLE widgets RENAME COLUMN note TO memo"])]);
        assert_eq!(f[0].severity, Severity::Warning);
    }
}
