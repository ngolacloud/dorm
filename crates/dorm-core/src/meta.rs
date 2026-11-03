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
    /// For a composite key this is the first key column — use
    /// [`primary_keys`](Self::primary_keys) when all of them are needed.
    pub fn primary_key(&self) -> Option<&'static FieldMeta> {
        self.fields.iter().find(|f| f.primary_key)
    }

    /// Every primary-key column, in declaration order — one for a simple key,
    /// several for a composite one (ADR 0042).
    pub fn primary_keys(&self) -> impl Iterator<Item = &'static FieldMeta> + '_ {
        self.fields.iter().filter(|f| f.primary_key)
    }

    /// Whether this model has a composite (multi-column) primary key.
    pub fn is_composite_pk(&self) -> bool {
        self.primary_keys().count() > 1
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

/// Marks one field of one model as its optimistic-locking version column.
///
/// Kept in a slice of its own rather than as a flag on [`FieldMeta`]: the version
/// is read only by the write seam (`save`/`update_if_version`), never by the pure
/// compiler, so it does not belong in the metadata the compiler reads — the same
/// discipline `dorm-rag` uses for `RagMeta`. `#[field(version)]` emits one of
/// these; [`ModelMeta::version_field`] resolves it back to the column (ADR 0023).
#[derive(Debug, Clone, Copy)]
pub struct VersionMeta {
    /// The model name, matching [`ModelMeta::name`].
    pub model: &'static str,
    /// The Rust field name carrying the version counter.
    pub field: &'static str,
}

inventory::collect!(VersionMeta);

impl ModelMeta {
    /// This model's optimistic-locking version field, if one was declared with
    /// `#[field(version)]`. Resolved through the [`VersionMeta`] slice, then the
    /// field looked up by name so the caller gets the full [`FieldMeta`].
    pub fn version_field(&self) -> Option<&'static FieldMeta> {
        let name = inventory::iter::<VersionMeta>
            .into_iter()
            .find(|v| v.model == self.name)?
            .field;
        self.field(name)
    }

    /// This model's soft-delete timestamp field, if declared with
    /// `#[model(soft_delete)]`. Resolved through the [`SoftDeleteMeta`] slice
    /// (ADR 0033), so `FieldMeta`/`ModelMeta` gain no field — the same discipline
    /// as `version`. A soft-delete model's rows are hidden by `deleted_at IS NULL`
    /// by default and removed only by a purge.
    pub fn soft_delete_field(&self) -> Option<&'static FieldMeta> {
        let name = inventory::iter::<SoftDeleteMeta>
            .into_iter()
            .find(|v| v.model == self.name)?
            .field;
        self.field(name)
    }

    /// This model's composite (multi-column) indexes, declared with
    /// `@@index([a, b])` in the schema or `#[model(index_together = "a, b")]` on
    /// the struct (ADR 0044). Each item is a group of column names.
    ///
    /// Resolved through the [`IndexMeta`] slice rather than a field on
    /// [`ModelMeta`] — the same discipline as `version`/`soft_delete`: a composite
    /// index is read only by the migrator (never by the pure compiler), so it does
    /// not belong in the metadata the compiler reads, and `ModelMeta` gains no
    /// field (and no ~30 literal constructors need touching).
    pub fn composite_indexes(&self) -> impl Iterator<Item = &'static [&'static str]> + '_ {
        inventory::iter::<IndexMeta>
            .into_iter()
            .filter(move |v| v.model == self.name)
            .map(|v| v.columns)
    }
}

/// Marks one composite index on a model: a group of column names to index
/// together (ADR 0044). In its own slice rather than on [`ModelMeta`], so the
/// metadata the compiler reads is unchanged; only the migrator reads it, the same
/// way `VersionMeta`/`SoftDeleteMeta` keep write-seam concerns out of the
/// compiler's view. One `#[model(index_together = "a, b")]` (or `@@index([a, b])`)
/// emits one of these.
#[derive(Debug, Clone, Copy)]
pub struct IndexMeta {
    /// The model name, matching [`ModelMeta::name`].
    pub model: &'static str,
    /// The column names of this index, in order.
    pub columns: &'static [&'static str],
}

inventory::collect!(IndexMeta);

/// Marks a model as soft-deleting, naming its `deleted_at`-style timestamp column
/// (ADR 0033). In its own slice rather than on [`ModelMeta`], so the metadata is
/// unchanged; the scope seam reads it to hide deleted rows by default.
#[derive(Debug, Clone, Copy)]
pub struct SoftDeleteMeta {
    pub model: &'static str,
    pub field: &'static str,
}

inventory::collect!(SoftDeleteMeta);

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

    // A versioned model, registered the way the derive does it: a `VersionMeta`
    // in the slice, naming the column. `version_field` resolves it back.
    inventory::submit! { VersionMeta { model: "Versioned", field: "rev" } }

    const VFIELDS: &[FieldMeta] = &[
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
            name: "rev",
            column: "rev",
            ty: FieldType::I64,
            nullable: false,
            primary_key: false,
            unique: false,
            index: false,
            max_length: None,
            default: Some("0"),
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
    ];

    const VMETA: ModelMeta = ModelMeta {
        name: "Versioned",
        table: "versioned",
        fields: VFIELDS,
        ordering: &[],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    };

    #[test]
    fn the_version_field_resolves_through_the_slice_and_is_absent_otherwise() {
        assert_eq!(VMETA.version_field().map(|f| f.name), Some("rev"));
        assert!(META.version_field().is_none());
    }

    #[test]
    fn on_delete_round_trips_through_its_attribute_spelling() {
        assert_eq!(OnDelete::parse("cascade"), Some(OnDelete::Cascade));
        assert_eq!(OnDelete::parse("set_null"), Some(OnDelete::SetNull));
        assert_eq!(OnDelete::parse("explode"), None);
        assert_eq!(OnDelete::SetNull.sql(), "SET NULL");
    }

    // Two composite indexes on one model, registered the way the derive does it:
    // one `IndexMeta` per group. `composite_indexes` resolves them back (ADR 0044).
    inventory::submit! { IndexMeta { model: "Indexed", columns: &["tenant", "created_at"] } }
    inventory::submit! { IndexMeta { model: "Indexed", columns: &["status"] } }

    #[test]
    fn composite_indexes_resolve_through_the_slice_and_are_empty_otherwise() {
        const IMETA: ModelMeta = ModelMeta {
            name: "Indexed",
            table: "indexed",
            fields: &[],
            ordering: &[],
            tenant_scoped: false,
            unique_together: &[],
            checks: &[],
        };
        let groups: Vec<Vec<&str>> = IMETA.composite_indexes().map(|g| g.to_vec()).collect();
        assert_eq!(groups, vec![vec!["tenant", "created_at"], vec!["status"]]);
        // A model with no `IndexMeta` submission has no composite indexes.
        assert_eq!(META.composite_indexes().count(), 0);
    }
}
