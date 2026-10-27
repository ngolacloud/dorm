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

/// The full schema for an ordered set of models: each `CREATE TABLE` followed by
/// its `CREATE INDEX`es. Statements are terminated by `;\n`, so a caller can run
/// them by splitting on `";\n"` (which is exactly what the integration harness
/// already does).
pub fn schema_for(models: &[&'static ModelMeta]) -> Result<String> {
    let mut out = String::new();
    for (i, m) in models.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&create_table(m)?);
        out.push_str(";\n");
        for idx in indexes(m) {
            out.push_str(&idx);
            out.push_str(";\n");
        }
    }
    Ok(out)
}

/// `CREATE TABLE IF NOT EXISTS <table> ( <columns>, <table constraints> )`.
///
/// Columns come first, in metadata order; then the table-level constraints —
/// composite `UNIQUE` groups, then `CHECK`s — matching the order the
/// hand-written schemas use.
pub fn create_table(meta: &ModelMeta) -> Result<String> {
    let mut parts =
        Vec::with_capacity(meta.fields.len() + meta.unique_together.len() + meta.checks.len());
    for f in meta.fields {
        parts.push(column_def(f)?);
    }
    for group in meta.unique_together {
        parts.push(format!("UNIQUE ({})", group.join(", ")));
    }
    for check in meta.checks {
        parts.push(format!("CHECK ({check})"));
    }
    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {} (\n    {}\n)",
        meta.table,
        parts.join(",\n    ")
    ))
}

/// One column definition.
///
/// A serial primary key is the `i64` PK with no explicit default — it becomes
/// `BIGSERIAL`. A primary key of any other type (a `TEXT` key like the DBaaS
/// `tenant_databases.addon_name`) keeps its own type: the "PK with no default ⇒
/// serial" shortcut that `FieldMeta::is_database_generated` uses on the *insert*
/// path would be wrong here, and the schema must not inherit that conflation.
fn column_def(f: &FieldMeta) -> Result<String> {
    if f.primary_key {
        if matches!(f.ty, FieldType::I64) && effective_default(f).is_none() && f.relation.is_none()
        {
            return Ok(format!("{} BIGSERIAL PRIMARY KEY", f.column));
        }
        let mut s = format!("{} {} PRIMARY KEY", f.column, column_type(f)?);
        if let Some(d) = effective_default(f) {
            s.push_str(&format!(" DEFAULT {d}"));
        }
        return Ok(s);
    }

    let mut s = format!("{} {}", f.column, column_type(f)?);
    if !f.nullable {
        s.push_str(" NOT NULL");
    }
    if f.unique {
        s.push_str(" UNIQUE");
    }
    if let Some(d) = effective_default(f) {
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

/// The column's SQL default, if any. An explicit `default = "..."` wins; failing
/// that, an `auto_now_add`/`auto_now` column gets `now()` as a database-level
/// safety net (the ORM also stamps the value in Rust on write, so this default
/// only ever applies if a row were inserted without the column — it never is).
/// This is exactly how the hand-written schema spelled these columns.
fn effective_default(f: &FieldMeta) -> Option<String> {
    f.default.map(ToString::to_string).or_else(|| {
        if f.auto_now_add || f.auto_now {
            Some("now()".to_string())
        } else {
            None
        }
    })
}

/// The SQL type for a column. A foreign key adopts the type of the primary key
/// it points at; every other column uses its own field type.
fn column_type(f: &FieldMeta) -> Result<String> {
    match f.relation {
        Some(rel) => {
            let target =
                registry::model(rel.model).map_err(|_| Error::UnknownRelationTarget(rel.model))?;
            let pk = target.primary_key().ok_or(Error::NoPrimaryKey(rel.model))?;
            Ok(pk.ty.sql_type().to_string())
        }
        None => Ok(f.ty.sql_type().to_string()),
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
        .map(|f| {
            format!(
                "CREATE INDEX IF NOT EXISTS {}_{}_idx ON {} ({})",
                meta.table, f.column, meta.table, f.column
            )
        })
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
        assert_eq!(column_def(&f).unwrap(), "id BIGSERIAL PRIMARY KEY");
    }

    #[test]
    fn a_text_primary_key_keeps_its_type_and_is_not_made_serial() {
        // This is the DBaaS `tenant_databases.addon_name` case. The insert-path
        // heuristic would call it database-generated; the schema must not.
        let f = FieldMeta {
            primary_key: true,
            ..field("addon_name", FieldType::Text)
        };
        assert_eq!(column_def(&f).unwrap(), "addon_name TEXT PRIMARY KEY");
    }

    #[test]
    fn a_not_null_unique_text_column_renders_both_modifiers_in_order() {
        let f = FieldMeta {
            unique: true,
            ..field("slug", FieldType::Text)
        };
        assert_eq!(column_def(&f).unwrap(), "slug TEXT NOT NULL UNIQUE");
    }

    #[test]
    fn a_default_follows_not_null() {
        let f = FieldMeta {
            default: Some("now()"),
            ..field("created_at", FieldType::Timestamp)
        };
        assert_eq!(
            column_def(&f).unwrap(),
            "created_at TIMESTAMPTZ NOT NULL DEFAULT now()"
        );
    }

    #[test]
    fn a_nullable_column_omits_not_null() {
        let f = FieldMeta {
            nullable: true,
            ..field("note", FieldType::Text)
        };
        assert_eq!(column_def(&f).unwrap(), "note TEXT");
    }

    #[test]
    fn jsonb_and_bigint_map_from_their_field_types() {
        assert_eq!(
            column_def(&field("doc", FieldType::Json)).unwrap(),
            "doc JSONB NOT NULL"
        );
        assert_eq!(
            column_def(&field("n", FieldType::I64)).unwrap(),
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
}
