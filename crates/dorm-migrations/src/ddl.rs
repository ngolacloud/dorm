//! Turn `&'static ModelMeta` into `CREATE TABLE`, indexes and foreign keys.
//!
//! Pure: no I/O, no clock, no global state. The output is deterministic in the
//! order of the models given and the order of fields in each model's metadata,
//! which is what lets it be tested by comparing text — the same discipline as
//! the query compiler's snapshot tests.

use crate::{Error, Result};
use dorm_core::meta::{FieldMeta, ModelMeta};
use dorm_core::registry;
use dorm_core::value::FieldType;

/// The SQL backend a schema is generated for. The portable core — the columns,
/// the `UNIQUE`/`CHECK`/`REFERENCES` constraints, the indexes — is the same; the
/// type names, the serial primary key and the `now()` default differ (ADR 0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Postgres,
    Sqlite,
    /// MySQL / MariaDB (ADR 0018). The type map below is the first cut; the
    /// `VARCHAR`-for-indexed-`TEXT` and table-level foreign-key refinements are
    /// the DDL slice (ADR 0018 Inc. 3).
    MySql,
}

impl Backend {
    /// The backend a DSN selects, using the ORM core's single detection rule so
    /// the migrator and the query layer never disagree. An empty or unrecognised
    /// DSN is PostgreSQL — the same default `dorm_core::db` keeps, which lets
    /// `dorm migrate make` author a migration with no database configured.
    pub fn of_dsn(dsn: &str) -> Backend {
        match dorm_core::db::backend_of(dsn) {
            dorm_core::db::Backend::Sqlite => Backend::Sqlite,
            dorm_core::db::Backend::Postgres => Backend::Postgres,
            dorm_core::db::Backend::MySql => Backend::MySql,
        }
    }
}

/// The full schema for an ordered set of models, for PostgreSQL. See
/// [`schema_for_on`] for another backend.
pub fn schema_for(models: &[&'static ModelMeta]) -> Result<String> {
    schema_for_on(models, Backend::Postgres)
}

/// The full schema for an ordered set of models, for `backend`: each
/// `CREATE TABLE` followed by its `CREATE INDEX`es, terminated by `;\n`.
pub fn schema_for_on(models: &[&'static ModelMeta], backend: Backend) -> Result<String> {
    let mut out = String::new();
    for (i, m) in models.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&create_table_on(m, backend)?);
        out.push_str(";\n");
        for idx in indexes(m) {
            out.push_str(&idx);
            out.push_str(";\n");
        }
    }
    Ok(out)
}

/// `CREATE TABLE …` for PostgreSQL. See [`create_table_on`] for another backend.
pub fn create_table(meta: &ModelMeta) -> Result<String> {
    create_table_on(meta, Backend::Postgres)
}

/// `CREATE TABLE IF NOT EXISTS <table> ( <columns>, <table constraints> )` for
/// `backend`.
///
/// Columns come first, in metadata order; then the table-level constraints —
/// composite `UNIQUE` groups, then `CHECK`s — matching the order the
/// hand-written schemas use.
pub fn create_table_on(meta: &ModelMeta, backend: Backend) -> Result<String> {
    create_table_named_on(meta, meta.table, backend)
}

/// `CREATE TABLE IF NOT EXISTS <name> ( … )` with `meta`'s columns and
/// constraints but an explicit table name — used by the autodetector's SQLite
/// table rebuild, which creates the new shape under a temporary name before
/// swapping it in. With `name == meta.table` this is exactly [`create_table_on`].
pub fn create_table_named_on(meta: &ModelMeta, name: &str, backend: Backend) -> Result<String> {
    let mut parts =
        Vec::with_capacity(meta.fields.len() + meta.unique_together.len() + meta.checks.len());
    // A composite primary key (>1 `primary_key` field) is a table-level
    // `PRIMARY KEY (a, b)` (ADR 0042); its columns are emitted plain (no inline
    // `PRIMARY KEY`). A single key stays inline, serial shortcut included.
    let pk_cols: Vec<&str> = meta
        .fields
        .iter()
        .filter(|f| f.primary_key)
        .map(|f| f.column)
        .collect();
    let composite = pk_cols.len() > 1;
    for f in meta.fields {
        if composite && f.primary_key {
            // A plain column — the key-ness is the table-level constraint below.
            parts.push(format!("{} {}", f.column, column_modifiers(f, backend)?));
        } else {
            parts.push(column_def(f, backend)?);
        }
    }
    if composite {
        parts.push(format!("PRIMARY KEY ({})", pk_cols.join(", ")));
    }
    for group in meta.unique_together {
        parts.push(format!("UNIQUE ({})", group.join(", ")));
    }
    for check in meta.checks {
        parts.push(format!("CHECK ({check})"));
    }
    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {} (\n    {}\n)",
        name,
        parts.join(",\n    ")
    ))
}

/// One column definition.
///
/// A serial primary key is the `i64` PK with no explicit default — `BIGSERIAL`
/// on PostgreSQL, `INTEGER PRIMARY KEY AUTOINCREMENT` on SQLite. A primary key of
/// any other type (a `TEXT` key like the DBaaS `tenant_databases.addon_name`)
/// keeps its own type: the "PK with no default ⇒ serial" shortcut that
/// `FieldMeta::is_database_generated` uses on the *insert* path would be wrong
/// here, and the schema must not inherit that conflation.
fn column_def(f: &FieldMeta, backend: Backend) -> Result<String> {
    if f.primary_key {
        if matches!(f.ty, FieldType::I64)
            && effective_default(f, backend).is_none()
            && f.relation.is_none()
        {
            return Ok(match backend {
                Backend::Postgres => format!("{} BIGSERIAL PRIMARY KEY", f.column),
                Backend::Sqlite => format!("{} INTEGER PRIMARY KEY AUTOINCREMENT", f.column),
                Backend::MySql => format!("{} BIGINT AUTO_INCREMENT PRIMARY KEY", f.column),
            });
        }
        let mut s = format!("{} {} PRIMARY KEY", f.column, column_type(f, backend)?);
        if let Some(d) = effective_default(f, backend) {
            s.push_str(&format!(" DEFAULT {d}"));
        }
        return Ok(s);
    }

    Ok(format!("{} {}", f.column, column_modifiers(f, backend)?))
}

/// Everything after the column name in a definition: the type, then
/// `NOT NULL` / `UNIQUE` / `DEFAULT` / `REFERENCES` in that order. Shared by the
/// `CREATE TABLE` column list and the `ALTER TABLE … ADD COLUMN` of [`add_column`]
/// so the two never diverge.
fn column_modifiers(f: &FieldMeta, backend: Backend) -> Result<String> {
    let mut s = column_type(f, backend)?;
    if !f.nullable {
        s.push_str(" NOT NULL");
    }
    if f.unique {
        s.push_str(" UNIQUE");
    }
    if let Some(d) = effective_default(f, backend) {
        s.push_str(&format!(" DEFAULT {d}"));
    }
    if let Some(rel) = f.relation {
        let (table, pk) = relation_target(rel.model)?;
        s.push_str(&format!(
            " REFERENCES {} ({}) ON DELETE {}",
            table,
            pk,
            rel.on_delete.sql()
        ));
    }
    Ok(s)
}

/// `ALTER TABLE <table> ADD COLUMN IF NOT EXISTS <column> <modifiers>` for one
/// field — the additive half of [`crate::sync`], which runs over PostgreSQL.
/// Idempotent, and never a primary key: adding a surrogate key to a populated
/// table is not a thing `sync` does.
///
/// A new `NOT NULL` column with no default is only safe on an empty table;
/// PostgreSQL will reject it otherwise, which is the right answer — `sync` is for
/// additive changes, not backfills.
pub fn add_column(meta: &ModelMeta, f: &FieldMeta) -> Result<String> {
    add_column_on(meta, f, Backend::Postgres)
}

/// `ALTER TABLE … ADD COLUMN …` for `backend` — the additive half of both
/// [`crate::sync`] and the autodetector. PostgreSQL gets `IF NOT EXISTS` (so the
/// statement is idempotent and can adopt a column that is already there); SQLite
/// does not support that clause on `ADD COLUMN`, so it is omitted there.
pub fn add_column_on(meta: &ModelMeta, f: &FieldMeta, backend: Backend) -> Result<String> {
    let guard = match backend {
        Backend::Postgres => "IF NOT EXISTS ",
        // MySQL has no `IF NOT EXISTS` on `ADD COLUMN`; the versioned ledger, not
        // idempotent re-runs, is the source of truth there (ADR 0018).
        Backend::Sqlite | Backend::MySql => "",
    };
    Ok(format!(
        "ALTER TABLE {} ADD COLUMN {}{} {}",
        meta.table,
        guard,
        f.column,
        column_modifiers(f, backend)?
    ))
}

/// `ALTER TABLE … DROP COLUMN …` for `backend`. PostgreSQL gets `IF EXISTS`;
/// SQLite does not support it on `DROP COLUMN`, so it is omitted there. The
/// autodetector only drops a column the previous snapshot recorded, so the
/// column is expected to be present.
pub fn drop_column(table: &str, column: &str, backend: Backend) -> String {
    let guard = match backend {
        Backend::Postgres => "IF EXISTS ",
        Backend::Sqlite | Backend::MySql => "",
    };
    format!("ALTER TABLE {table} DROP COLUMN {guard}{column}")
}

/// `DROP TABLE IF EXISTS …` — the same on both backends.
pub fn drop_table(table: &str) -> String {
    format!("DROP TABLE IF EXISTS {table}")
}

/// `DROP INDEX IF EXISTS …` for the index [`index_on`] would have created.
pub fn drop_index(table: &str, column: &str) -> String {
    format!("DROP INDEX IF EXISTS {}_{}_idx", table, column)
}

/// `ALTER TABLE <table> RENAME COLUMN <from> TO <to>` — the data-preserving
/// alternative to drop-then-add when a column is renamed (B-24). Both PostgreSQL
/// and SQLite (≥ 3.25) accept this exact form, so no dialect split is needed.
pub fn rename_column(table: &str, from: &str, to: &str) -> String {
    format!("ALTER TABLE {table} RENAME COLUMN {from} TO {to}")
}

/// Reject anything that is not a plain SQL identifier before it is spliced into
/// DDL. The expand/contract builders below take table/column/constraint names as
/// *configuration*, never bound parameters, so each one passes through here — the
/// same gate `partition.rs` keeps (ADR 0022: identifiers come from the schema,
/// not from a caller; **I1**).
fn ident(name: &str) -> Result<&str> {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(name)
    } else {
        Err(Error::Orm(dorm_core::Error::Config(format!(
            "`{name}` is not a usable SQL identifier"
        ))))
    }
}

/// The conventional name of the `NOT NULL` guard constraint for a column:
/// `<table>_<column>_not_null`. The one place it is spelled, so the `ADD` of
/// [`add_not_null_check_not_valid`] and the `VALIDATE`/`DROP` of
/// [`validate_not_null`]/[`drop_constraint`] always agree on the name.
pub fn not_null_constraint(table: &str, column: &str) -> String {
    format!("{table}_{column}_not_null")
}

/// `ALTER TABLE <t> ADD CONSTRAINT <t>_<c>_not_null CHECK (<c> IS NOT NULL) NOT VALID`
/// — the first step of enforcing `NOT NULL` online (ADR 0038 / expand/contract).
///
/// `NOT VALID` means PostgreSQL adds the constraint **without scanning the table**,
/// taking only a brief lock: new and updated rows are checked immediately, but the
/// existing rows are left for a later [`validate_not_null`]. That split is what
/// lets a populated table gain a `NOT NULL` guard without a long `ACCESS EXCLUSIVE`
/// lock or a table rewrite — unlike `ALTER COLUMN … SET NOT NULL`, which scans the
/// whole table under an exclusive lock.
pub fn add_not_null_check_not_valid(table: &str, column: &str) -> Result<String> {
    let table = ident(table)?;
    let column = ident(column)?;
    let name = not_null_constraint(table, column);
    Ok(format!(
        "ALTER TABLE {table} ADD CONSTRAINT {name} CHECK ({column} IS NOT NULL) NOT VALID"
    ))
}

/// `ALTER TABLE <t> VALIDATE CONSTRAINT <t>_<c>_not_null` — the second step.
///
/// Validation scans the table to prove every existing row satisfies the check, but
/// under a `SHARE UPDATE EXCLUSIVE` lock that lets reads and writes keep flowing —
/// the lock a non-blocking operation can afford. Run it after the backfill has
/// filled the column, so the scan finds no violations.
pub fn validate_not_null(table: &str, column: &str) -> Result<String> {
    let table = ident(table)?;
    let column = ident(column)?;
    let name = not_null_constraint(table, column);
    Ok(format!("ALTER TABLE {table} VALIDATE CONSTRAINT {name}"))
}

/// `ALTER TABLE <t> ADD CONSTRAINT <name> CHECK (<expr>) NOT VALID` for an
/// arbitrary author-written `check_expr` — the general form of
/// [`add_not_null_check_not_valid`]. The check expression is the author's SQL (the
/// same trust `ModelMeta::checks` and ADR 0005 raw SQL already carry); `table` and
/// `name` are validated identifiers.
pub fn add_check_not_valid(table: &str, name: &str, check_expr: &str) -> Result<String> {
    let table = ident(table)?;
    let name = ident(name)?;
    Ok(format!(
        "ALTER TABLE {table} ADD CONSTRAINT {name} CHECK ({check_expr}) NOT VALID"
    ))
}

/// `ALTER TABLE <t> VALIDATE CONSTRAINT <name>` for an arbitrary constraint name.
pub fn validate_constraint(table: &str, name: &str) -> Result<String> {
    let table = ident(table)?;
    let name = ident(name)?;
    Ok(format!("ALTER TABLE {table} VALIDATE CONSTRAINT {name}"))
}

/// `ALTER TABLE <t> DROP CONSTRAINT IF EXISTS <name>` — the `contract`-step inverse
/// of an added constraint, or the cleanup once the column is a real `NOT NULL`.
pub fn drop_constraint(table: &str, name: &str) -> Result<String> {
    let table = ident(table)?;
    let name = ident(name)?;
    Ok(format!(
        "ALTER TABLE {table} DROP CONSTRAINT IF EXISTS {name}"
    ))
}

/// `ALTER TABLE <t> ALTER COLUMN <c> SET NOT NULL` — the optional final step.
///
/// On PostgreSQL 12+ a *validated* `CHECK (<c> IS NOT NULL)` lets this `SET NOT NULL`
/// be proven from the constraint and **skip the full-table scan**, so the column
/// becomes a genuine `NOT NULL` cheaply; the guard check can then be dropped with
/// [`drop_constraint`]. Run it only after [`validate_not_null`] has succeeded.
pub fn set_not_null(table: &str, column: &str) -> Result<String> {
    let table = ident(table)?;
    let column = ident(column)?;
    Ok(format!(
        "ALTER TABLE {table} ALTER COLUMN {column} SET NOT NULL"
    ))
}

/// `CREATE INDEX IF NOT EXISTS <table>_<column>_idx ON <table> (<column>)` — the
/// one place the index name is spelled, shared by [`indexes`] and the
/// autodetector so a dropped index names exactly what a created one made.
pub fn index_on(table: &str, column: &str) -> String {
    format!("CREATE INDEX IF NOT EXISTS {table}_{column}_idx ON {table} ({column})")
}

/// Like [`index_on`] but non-blocking on PostgreSQL (B-22).
///
/// A plain `CREATE INDEX` takes a `SHARE` lock that stops writes to the table
/// for the whole build; `CREATE INDEX CONCURRENTLY` takes only a
/// `SHARE UPDATE EXCLUSIVE` lock, so writes keep flowing while the index builds.
/// The trade-off is that `CONCURRENTLY` **cannot run inside a transaction**
/// (PostgreSQL rejects it with `25001`), so the caller must apply it outside one
/// — which is why [`crate::migrate`]/`sync` run indexes separately from the
/// transactional table changes. SQLite has no `CONCURRENTLY`, so it falls back
/// to the plain form.
pub fn index_on_concurrent(table: &str, column: &str, backend: Backend) -> String {
    match backend {
        // MySQL has no `CREATE INDEX CONCURRENTLY`.
        Backend::MySql => index_on(table, column),
        Backend::Postgres => format!(
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS {table}_{column}_idx ON {table} ({column})"
        ),
        Backend::Sqlite => index_on(table, column),
    }
}

/// The column's SQL default, if any. An explicit `default = "..."` wins; failing
/// that, an `auto_now_add`/`auto_now` column gets the backend's "now" as a
/// database-level safety net (the ORM also stamps the value in Rust on write, so
/// this default only ever applies if a row were inserted without the column — it
/// never is).
fn effective_default(f: &FieldMeta, backend: Backend) -> Option<String> {
    f.default.map(ToString::to_string).or_else(|| {
        if f.auto_now_add || f.auto_now {
            Some(match backend {
                Backend::Postgres => "now()".to_string(),
                Backend::Sqlite => "CURRENT_TIMESTAMP".to_string(),
                Backend::MySql => "CURRENT_TIMESTAMP(6)".to_string(),
            })
        } else {
            None
        }
    })
}

/// The SQL type for a column on `backend`. A foreign key adopts the type of the
/// primary key it points at; every other column uses its own field type.
fn column_type(f: &FieldMeta, backend: Backend) -> Result<String> {
    let ty = match f.relation {
        Some(rel) => {
            let target =
                registry::model(rel.model).map_err(|_| Error::UnknownRelationTarget(rel.model))?;
            target
                .primary_key()
                .ok_or(Error::NoPrimaryKey(rel.model))?
                .ty
        }
        None => f.ty,
    };
    Ok(sql_type(ty, backend))
}

/// A field type's column type on `backend`. PostgreSQL uses the ORM core's
/// mapping; SQLite collapses onto its storage classes (ADR 0002).
fn sql_type(ty: FieldType, backend: Backend) -> String {
    match backend {
        Backend::Postgres => ty.sql_type().to_string(),
        Backend::Sqlite => match ty {
            FieldType::I32 | FieldType::I64 | FieldType::Bool => "INTEGER",
            FieldType::Text | FieldType::Timestamp | FieldType::Uuid | FieldType::Json => "TEXT",
            FieldType::Decimal => "NUMERIC",
            FieldType::F64 => "REAL",
            FieldType::Bytes => "BLOB",
            // SQLite has no INET; the text form is stored, and a read is refused
            // (ADR 0040) — network operators are PostgreSQL-only.
            FieldType::Inet => "TEXT",
            FieldType::TextArray | FieldType::IntArray => "TEXT",
        }
        .to_string(),
        Backend::MySql => match ty {
            FieldType::I32 => "INT",
            FieldType::I64 => "BIGINT",
            FieldType::F64 => "DOUBLE",
            // TINYINT(1) is MySQL's boolean; the driver maps 0/1 ↔ bool.
            FieldType::Bool => "TINYINT(1)",
            // `TEXT` here; a UNIQUE/indexed TEXT needs a `VARCHAR(n)` — the Inc. 3
            // refinement that has the field's attributes to decide.
            FieldType::Text => "TEXT",
            // An unqualified DECIMAL is `(10,0)` on MySQL and would truncate cents;
            // a wide precision keeps money exact, as ADR 0016 requires.
            FieldType::Decimal => "DECIMAL(38,9)",
            // DATETIME (not TIMESTAMP: no 2038 limit, no tz conversion); UTC comes
            // from the connection's `time_zone='+00:00'`.
            FieldType::Timestamp => "DATETIME(6)",
            FieldType::Uuid => "CHAR(36)",
            FieldType::Json => "JSON",
            FieldType::Bytes => "LONGBLOB",
            FieldType::Inet => "TEXT",
            FieldType::TextArray | FieldType::IntArray => "TEXT",
        }
        .to_string(),
    }
}

/// `(target table, target primary-key column)` for a relation's model name.
fn relation_target(model: &'static str) -> Result<(&'static str, &'static str)> {
    let target = registry::model(model).map_err(|_| Error::UnknownRelationTarget(model))?;
    let pk = target.primary_key().ok_or(Error::NoPrimaryKey(model))?;
    Ok((target.table, pk.column))
}

/// `CREATE INDEX IF NOT EXISTS <table>_<column>_idx ...` for each indexed field.
/// A `unique` column already has an index; this mirrors the hand-written schema,
/// which declares the explicit index anyway when the field asks for one.
pub fn indexes(meta: &ModelMeta) -> Vec<String> {
    meta.fields
        .iter()
        .filter(|f| f.index)
        .map(|f| index_on(meta.table, f.column))
        .collect()
}

/// Like [`indexes`] but each index builds `CONCURRENTLY` on PostgreSQL (B-22), so
/// the caller must run these outside a transaction. Used by `sync`, which applies
/// indexes to a possibly-populated table where a blocking build would stop
/// writes.
pub fn indexes_concurrent(meta: &ModelMeta, backend: Backend) -> Vec<String> {
    meta.fields
        .iter()
        .filter(|f| f.index)
        .map(|f| index_on_concurrent(meta.table, f.column, backend))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-built metadata so these tests need no registered models (and no
    // derive): they exercise the column renderer directly, the way `meta.rs`
    // builds a `ModelMeta` by hand for its own tests.
    fn field(name: &'static str, ty: FieldType) -> FieldMeta {
        FieldMeta {
            name,
            column: name,
            ty,
            nullable: false,
            primary_key: false,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        }
    }

    #[test]
    fn an_i64_primary_key_without_a_default_becomes_bigserial() {
        let f = FieldMeta {
            primary_key: true,
            ..field("id", FieldType::I64)
        };
        assert_eq!(
            column_def(&f, Backend::Postgres).unwrap(),
            "id BIGSERIAL PRIMARY KEY"
        );
    }

    #[test]
    fn a_text_primary_key_keeps_its_type_and_is_not_made_serial() {
        // This is the DBaaS `tenant_databases.addon_name` case. The insert-path
        // heuristic would call it database-generated; the schema must not.
        let f = FieldMeta {
            primary_key: true,
            ..field("addon_name", FieldType::Text)
        };
        assert_eq!(
            column_def(&f, Backend::Postgres).unwrap(),
            "addon_name TEXT PRIMARY KEY"
        );
    }

    #[test]
    fn a_not_null_unique_text_column_renders_both_modifiers_in_order() {
        let f = FieldMeta {
            unique: true,
            ..field("slug", FieldType::Text)
        };
        assert_eq!(
            column_def(&f, Backend::Postgres).unwrap(),
            "slug TEXT NOT NULL UNIQUE"
        );
    }

    #[test]
    fn a_default_follows_not_null() {
        let f = FieldMeta {
            default: Some("now()"),
            ..field("created_at", FieldType::Timestamp)
        };
        assert_eq!(
            column_def(&f, Backend::Postgres).unwrap(),
            "created_at TIMESTAMPTZ NOT NULL DEFAULT now()"
        );
    }

    #[test]
    fn a_nullable_column_omits_not_null() {
        let f = FieldMeta {
            nullable: true,
            ..field("note", FieldType::Text)
        };
        assert_eq!(column_def(&f, Backend::Postgres).unwrap(), "note TEXT");
    }

    #[test]
    fn jsonb_and_bigint_map_from_their_field_types() {
        assert_eq!(
            column_def(&field("doc", FieldType::Json), Backend::Postgres).unwrap(),
            "doc JSONB NOT NULL"
        );
        assert_eq!(
            column_def(&field("n", FieldType::I64), Backend::Postgres).unwrap(),
            "n BIGINT NOT NULL"
        );
    }

    #[test]
    fn an_index_is_named_table_column_idx() {
        const FS: &[FieldMeta] = &[FieldMeta {
            name: "slug",
            column: "slug",
            ty: FieldType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            index: true,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        }];
        const M: ModelMeta = ModelMeta {
            name: "T",
            table: "tenants",
            fields: FS,
            ordering: &[],
            tenant_scoped: false,
            unique_together: &[],
            checks: &[],
        };
        assert_eq!(
            indexes(&M),
            vec!["CREATE INDEX IF NOT EXISTS tenants_slug_idx ON tenants (slug)"]
        );
    }

    // ADR 0038: the expand/contract vocabulary — `CHECK … NOT VALID` then
    // `VALIDATE` — turns a nullable column into a `NOT NULL` one without a long
    // exclusive lock, and names the guard constraint the same way every time.
    #[test]
    fn a_not_null_check_is_added_not_valid_then_validated_under_a_stable_name() {
        assert_eq!(
            add_not_null_check_not_valid("accounts", "email").unwrap(),
            "ALTER TABLE accounts ADD CONSTRAINT accounts_email_not_null \
             CHECK (email IS NOT NULL) NOT VALID"
        );
        assert_eq!(
            validate_not_null("accounts", "email").unwrap(),
            "ALTER TABLE accounts VALIDATE CONSTRAINT accounts_email_not_null"
        );
        assert_eq!(
            set_not_null("accounts", "email").unwrap(),
            "ALTER TABLE accounts ALTER COLUMN email SET NOT NULL"
        );
        assert_eq!(
            drop_constraint("accounts", "accounts_email_not_null").unwrap(),
            "ALTER TABLE accounts DROP CONSTRAINT IF EXISTS accounts_email_not_null"
        );
    }

    #[test]
    fn a_general_check_carries_the_author_expression_and_validates_by_name() {
        assert_eq!(
            add_check_not_valid("orders", "orders_total_positive", "total >= 0").unwrap(),
            "ALTER TABLE orders ADD CONSTRAINT orders_total_positive CHECK (total >= 0) NOT VALID"
        );
        assert_eq!(
            validate_constraint("orders", "orders_total_positive").unwrap(),
            "ALTER TABLE orders VALIDATE CONSTRAINT orders_total_positive"
        );
    }

    #[test]
    fn an_injection_in_a_constraint_table_or_column_name_is_refused() {
        assert!(add_not_null_check_not_valid("evil; DROP TABLE x", "c").is_err());
        assert!(add_not_null_check_not_valid("t", "c; DROP").is_err());
        assert!(add_check_not_valid("t", "name; DROP", "c IS NOT NULL").is_err());
        assert!(validate_constraint("t", "bad name").is_err());
        assert!(set_not_null("", "c").is_err());
    }

    // B-22: the concurrent variant builds without blocking writes on PostgreSQL
    // and falls back to the plain form on SQLite (which has no CONCURRENTLY).
    #[test]
    fn a_concurrent_index_is_non_blocking_on_postgres_and_plain_on_sqlite() {
        assert_eq!(
            index_on_concurrent("tenants", "slug", Backend::Postgres),
            "CREATE INDEX CONCURRENTLY IF NOT EXISTS tenants_slug_idx ON tenants (slug)"
        );
        assert_eq!(
            index_on_concurrent("tenants", "slug", Backend::Sqlite),
            "CREATE INDEX IF NOT EXISTS tenants_slug_idx ON tenants (slug)"
        );
    }

    #[test]
    fn a_whole_table_without_relations_renders_as_expected() {
        // The DBaaS `tenant_databases` shape, minus the composite UNIQUE and the
        // CHECKs the metadata cannot yet express (see the orm-models parity test).
        const FS: &[FieldMeta] = &[
            FieldMeta {
                name: "addon_name",
                column: "addon_name",
                ty: FieldType::Text,
                nullable: false,
                primary_key: true,
                unique: false,
                index: false,
                max_length: None,
                default: None,
                auto_now_add: false,
                auto_now: false,
                relation: None,
            },
            FieldMeta {
                name: "token_sha256",
                column: "token_sha256",
                ty: FieldType::Text,
                nullable: false,
                primary_key: false,
                unique: true,
                index: false,
                max_length: None,
                default: None,
                auto_now_add: false,
                auto_now: false,
                relation: None,
            },
            FieldMeta {
                name: "doc",
                column: "doc",
                ty: FieldType::Json,
                nullable: false,
                primary_key: false,
                unique: false,
                index: false,
                max_length: None,
                default: None,
                auto_now_add: false,
                auto_now: false,
                relation: None,
            },
        ];
        const M: ModelMeta = ModelMeta {
            name: "TenantDatabase",
            table: "tenant_databases",
            fields: FS,
            ordering: &[],
            tenant_scoped: true,
            unique_together: &[&["tenant", "name"]],
            checks: &["token_sha256 ~ '^sha256:[0-9a-f]{64}$'"],
        };
        assert_eq!(
            create_table(&M).unwrap(),
            "CREATE TABLE IF NOT EXISTS tenant_databases (\n    \
             addon_name TEXT PRIMARY KEY,\n    \
             token_sha256 TEXT NOT NULL UNIQUE,\n    \
             doc JSONB NOT NULL,\n    \
             UNIQUE (tenant, name),\n    \
             CHECK (token_sha256 ~ '^sha256:[0-9a-f]{64}$')\n)"
        );
    }

    #[test]
    fn table_constraints_come_after_the_columns_unique_before_check() {
        const FS: &[FieldMeta] = &[FieldMeta {
            name: "a",
            column: "a",
            ty: FieldType::Text,
            nullable: false,
            primary_key: true,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        }];
        const M: ModelMeta = ModelMeta {
            name: "X",
            table: "x",
            fields: FS,
            ordering: &[],
            tenant_scoped: false,
            unique_together: &[&["a", "b"], &["c"]],
            checks: &["a <> ''"],
        };
        assert_eq!(
            create_table(&M).unwrap(),
            "CREATE TABLE IF NOT EXISTS x (\n    \
             a TEXT PRIMARY KEY,\n    \
             UNIQUE (a, b),\n    \
             UNIQUE (c),\n    \
             CHECK (a <> '')\n)"
        );
    }

    // A small model used by the SQLite dialect tests: a serial key, a text
    // column, and an `auto_now_add` timestamp.
    const NOTE_FIELDS: &[FieldMeta] = &[
        FieldMeta {
            name: "id",
            column: "id",
            ty: FieldType::I64,
            nullable: false,
            primary_key: true,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
        FieldMeta {
            name: "title",
            column: "title",
            ty: FieldType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            index: false,
            max_length: Some(200),
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
        FieldMeta {
            name: "created_at",
            column: "created_at",
            ty: FieldType::Timestamp,
            nullable: false,
            primary_key: false,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: true,
            auto_now: false,
            relation: None,
        },
    ];
    const NOTE: ModelMeta = ModelMeta {
        name: "Note",
        table: "notes",
        fields: NOTE_FIELDS,
        ordering: &[],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    };

    #[test]
    fn sqlite_maps_the_serial_key_the_types_and_the_now_default() {
        let ddl = schema_for_on(&[&NOTE], Backend::Sqlite).unwrap();
        assert!(
            ddl.contains("id INTEGER PRIMARY KEY AUTOINCREMENT"),
            "{ddl}"
        );
        assert!(ddl.contains("title TEXT NOT NULL"), "{ddl}");
        assert!(
            ddl.contains("created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP"),
            "{ddl}"
        );
        // The PostgreSQL spelling must be absent.
        assert!(!ddl.contains("BIGSERIAL"), "{ddl}");
        assert!(!ddl.contains("TIMESTAMPTZ"), "{ddl}");
    }

    #[test]
    fn sqlite_schema_creates_a_working_database() {
        // The real proof: the generated SQLite DDL creates a usable schema, and a
        // row round-trips — the serial key assigns, the `now()` default fills.
        let ddl = schema_for_on(&[&NOTE], Backend::Sqlite).unwrap();
        let conn = rusqlite::Connection::open_in_memory().expect("open sqlite");
        conn.execute_batch(&ddl).expect("apply schema");
        conn.execute("INSERT INTO notes (title) VALUES ('hello')", [])
            .expect("insert");
        let (id, title): (i64, String) = conn
            .query_row("SELECT id, title FROM notes", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .expect("select");
        assert_eq!(id, 1);
        assert_eq!(title, "hello");
        let with_ts: i64 = conn
            .query_row(
                "SELECT count(*) FROM notes WHERE created_at IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(
            with_ts, 1,
            "the CURRENT_TIMESTAMP default filled created_at"
        );
    }
}
