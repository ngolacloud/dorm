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
    for f in meta.fields {
        parts.push(column_def(f, backend)?);
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
        Backend::Sqlite => "",
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
        Backend::Sqlite => "",
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

/// `CREATE INDEX IF NOT EXISTS <table>_<column>_idx ON <table> (<column>)` — the
/// one place the index name is spelled, shared by [`indexes`] and the
/// autodetector so a dropped index names exactly what a created one made.
pub fn index_on(table: &str, column: &str) -> String {
    format!("CREATE INDEX IF NOT EXISTS {table}_{column}_idx ON {table} ({column})")
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
