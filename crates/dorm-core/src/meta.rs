//! Model metadata — the single description both front ends read.
//!
//! Everything here is `&'static`, built by `#[derive(Model)]` at compile time and
//! submitted into a distributed slice. That shape is the point: the typed API
//! reaches it through generics, the REPL reaches the *same* values by iterating
//! the slice, and neither needs a hand-written registration list. Adding a model
//! is a recompile, not an edit in two places.

use crate::value::FieldType;

/// What happens to the rows on the many side when the one side is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDelete {
    Cascade,
    Restrict,
    SetNull,
    DoNothing,
}

impl OnDelete {
    /// The SQL clause, for the migration autodetector.
    pub fn sql(self) -> &'static str {
        match self {
            OnDelete::Cascade => "CASCADE",
            OnDelete::Restrict => "RESTRICT",
            OnDelete::SetNull => "SET NULL",
            OnDelete::DoNothing => "NO ACTION",
        }
    }

    /// Parse the `on_delete = "..."` attribute value.
    pub fn parse(s: &str) -> Option<OnDelete> {
        Some(match s {
            "cascade" => OnDelete::Cascade,
            "restrict" => OnDelete::Restrict,
            "set_null" => OnDelete::SetNull,
            "do_nothing" => OnDelete::DoNothing,
            _ => return None,
        })
    }
}

/// A foreign key's target, as named in the attribute.
#[derive(Debug, Clone, Copy)]
pub struct Relation {
    /// The *model* name, e.g. `"Tenant"` — not the table. Resolved through the
    /// registry at query-compile time, which is what allows the two models to be
    /// derived independently and in any order.
    pub model: &'static str,
    pub on_delete: OnDelete,
    /// The accessor installed on the far side, e.g. `"apps"` on `Tenant`.
    pub related_name: Option<&'static str>,
}

/// One column.
#[derive(Debug, Clone, Copy)]
pub struct FieldMeta {
    /// The Rust struct field name, which is also what a lookup path names.
    pub name: &'static str,
    /// The database column. Defaults to `name`, overridable per field.
    pub column: &'static str,
    pub ty: FieldType,
    pub nullable: bool,
    pub primary_key: bool,
    pub unique: bool,
    pub index: bool,
    pub max_length: Option<u32>,
    /// A raw SQL default, used only by the autodetector when creating the column.
    pub default: Option<&'static str>,
    /// Set to `now()` on insert, and left alone afterwards.
    pub auto_now_add: bool,
    /// Set to `now()` on every save.
    pub auto_now: bool,
    pub relation: Option<Relation>,
}

impl FieldMeta {
    /// Whether the database, not the caller, supplies this column's value on
    /// insert. Such a column is left out of the `INSERT` column list.
    pub fn is_database_generated(&self) -> bool {
        // The serial/identity case is an INTEGER primary key with no default.
        // A primary key of any other type — a `TEXT` key like the DBaaS
        // `tenant_databases.addon_name` — is supplied by the caller, so it must
        // stay in the INSERT list. Treating every keyless PK as serial (the old
        // rule) silently dropped a text key and broke the insert.
        let serial_pk = self.primary_key
            && self.default.is_none()
            && matches!(self.ty, FieldType::I32 | FieldType::I64);
        serial_pk || self.default.is_some()
    }

    /// Whether this field holds a foreign key.
    pub fn is_relation(&self) -> bool {
        self.relation.is_some()
    }
}

/// One model.
#[derive(Debug, Clone, Copy)]
pub struct ModelMeta {
    /// The name used in Rust and typed in the REPL, e.g. `"Tenant"`.
    pub name: &'static str,
    pub table: &'static str,
    pub fields: &'static [FieldMeta],
    /// Default ordering, each entry a field name optionally prefixed `-`.
    pub ordering: &'static [&'static str],
    /// Whether `--tenant <slug>` should constrain this model automatically.
    pub tenant_scoped: bool,
    /// Composite uniqueness: each group is a set of **column** names that must
    /// be unique together, e.g. `&[&["tenant", "name"]]`. A table-level
    /// constraint the migration autodetector emits; per-column `unique` lives on
    /// [`FieldMeta`].
    pub unique_together: &'static [&'static [&'static str]],
    /// Table-level `CHECK` predicates, as raw SQL (without the `CHECK (...)`
    /// wrapper), e.g. `"doc->>'tenant' = tenant"`. These come from the model
    /// definition at compile time, never from a caller — the same rule as a
    /// field default — so the autodetector may quote them straight into the DDL.
    pub checks: &'static [&'static str],
}

impl ModelMeta {
    /// Look a field up by its Rust name.
    pub fn field(&self, name: &str) -> Option<&'static FieldMeta> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The primary key field. A model without one cannot be saved or deleted by
    /// instance, so the rest of the code asks for it rather than assuming `id`.
    pub fn primary_key(&self) -> Option<&'static FieldMeta> {
        self.fields.iter().find(|f| f.primary_key)
    }

    /// The field holding the tenant, for `tenant_scoped` models.
    pub fn tenant_field(&self) -> Option<&'static FieldMeta> {
        if !self.tenant_scoped {
            return None;
        }
        // Either a column literally called `tenant`, or the single foreign key
        // pointing at a model of that name. Both spellings appear in practice.
        self.field("tenant").or_else(|| {
            self.fields
                .iter()
                .find(|f| f.relation.is_some_and(|r| r.model == "Tenant"))
        })
    }

    /// The columns a `SELECT *` should name, in metadata order.
    ///
    /// They are always named explicitly, never `*`: a `SELECT *` makes the row
    /// decoder depend on the physical column order, which is exactly the defect
    /// already living at `crates/delonix-api/src/cpstore/store_api_keys.rs:42`,
    /// where a column added in the middle of the list silently mis-maps fields.
    pub fn columns(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.fields.iter().map(|f| f.column)
    }
}

// The distributed slice. Each `#[derive(Model)]` emits one `inventory::submit!`,
// and the linker gathers them; nothing enumerates models by hand.
inventory::collect!(ModelMeta);

#[cfg(test)]
mod tests {
    use super::*;

    const FIELDS: &[FieldMeta] = &[
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
            name: "slug",
            column: "slug",
            ty: FieldType::Text,
            nullable: false,
            primary_key: false,
            unique: true,
            index: false,
            max_length: Some(63),
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
            default: Some("now()"),
            auto_now_add: true,
            auto_now: false,
            relation: None,
        },
    ];

    const META: ModelMeta = ModelMeta {
        name: "Tenant",
        table: "tenants",
        fields: FIELDS,
        ordering: &["-created_at"],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    };

    #[test]
    fn a_field_is_found_by_its_rust_name_and_an_unknown_one_is_not() {
        assert_eq!(META.field("slug").unwrap().column, "slug");
        assert!(META.field("slog").is_none());
    }

    #[test]
    fn the_primary_key_is_reported_rather_than_assumed_to_be_id() {
        assert_eq!(META.primary_key().unwrap().name, "id");
    }

    #[test]
    fn a_serial_primary_key_and_a_defaulted_column_are_both_database_generated() {
        // Both must be left out of the INSERT column list.
        assert!(FIELDS[0].is_database_generated());
        assert!(FIELDS[2].is_database_generated());
        // A plain column is not.
        assert!(!FIELDS[1].is_database_generated());
    }

    #[test]
    fn a_text_primary_key_is_caller_supplied_not_database_generated() {
        // The DBaaS `tenant_databases.addon_name` case: a keyless PK that is NOT
        // an integer is the caller's value and must stay in the INSERT list.
        let text_pk = FieldMeta {
            primary_key: true,
            ..FIELDS[1]
        };
        assert!(text_pk.primary_key && !text_pk.is_database_generated());
    }

    #[test]
    fn columns_come_out_in_metadata_order() {
        assert_eq!(
            META.columns().collect::<Vec<_>>(),
            vec!["id", "slug", "created_at"]
        );
    }

    #[test]
    fn a_model_that_is_not_tenant_scoped_has_no_tenant_field() {
        assert!(META.tenant_field().is_none());
    }

    #[test]
    fn on_delete_round_trips_through_its_attribute_spelling() {
        assert_eq!(OnDelete::parse("cascade"), Some(OnDelete::Cascade));
        assert_eq!(OnDelete::parse("set_null"), Some(OnDelete::SetNull));
        assert_eq!(OnDelete::parse("explode"), None);
        assert_eq!(OnDelete::SetNull.sql(), "SET NULL");
    }
}
