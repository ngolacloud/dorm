//! The example models: Tenant, Plan, Application, Deployment.
//!
//! These exist so the ORM has something real to be tested against, and so the
//! REPL has models to enumerate. They are **not** the platform's own tables:
//! `delonix-api` keeps its state through `CpStore`
//! (`crates/delonix-api/src/cpstore/store_base.rs`), with its own versioned
//! migrator, and this crate deliberately does not touch that schema. Keeping
//! them apart is what stops two migrators from meeting in one database.
//!
//! Note what defining a model costs: one `#[derive(Model)]`, and the model is in
//! the registry — queryable from Rust, listed by `:models` in the shell, and
//! available to the migration autodetector. There is no central file to edit.

use chrono::{DateTime, Utc};
use dorm_core::field::ForeignKey;
use dorm_derive::Model;
use rust_decimal::Decimal;

/// A customer of the platform.
#[derive(Model, Debug, Clone)]
#[model(table = "tenants", ordering = "-created_at")]
pub struct Tenant {
    #[field(primary_key)]
    pub id: i64,
    /// The short name used in URLs and on the command line.
    #[field(unique, index, max_length = 63)]
    pub slug: String,
    #[field(max_length = 200)]
    pub name: String,
    #[field(auto_now_add)]
    pub created_at: DateTime<Utc>,
}

/// A billing plan.
#[derive(Model, Debug, Clone)]
#[model(table = "plans", ordering = "code")]
pub struct Plan {
    #[field(primary_key)]
    pub id: i64,
    #[field(unique, max_length = 32)]
    pub code: String,
    #[field(max_length = 120)]
    pub name: String,
    /// A `Decimal`, not an `f64`: a monthly price that loses a cent to binary
    /// rounding is a billing defect, and `NUMERIC` is what PostgreSQL offers to
    /// prevent it.
    pub monthly_price: Decimal,
}

/// An application a tenant runs.
#[derive(Model, Debug, Clone)]
#[model(table = "applications", ordering = "-created_at", tenant_scoped)]
pub struct Application {
    #[field(primary_key)]
    pub id: i64,
    /// `related_name = "apps"` is what makes `Count("apps")` work from the
    /// `Tenant` side, and what the reverse lookup in the registry resolves.
    #[field(foreign_key = "Tenant", on_delete = "cascade", related_name = "apps")]
    pub tenant: ForeignKey<Tenant>,
    #[field(
        foreign_key = "Plan",
        on_delete = "restrict",
        related_name = "applications"
    )]
    pub plan: ForeignKey<Plan>,
    #[field(index, max_length = 63)]
    pub name: String,
    #[field(auto_now_add)]
    pub created_at: DateTime<Utc>,
}

/// One rollout of an application.
///
/// Not marked `tenant_scoped`: it reaches its tenant only through
/// `app__tenant`, and a `--tenant` filter wants a field it can constrain
/// directly. Saying so here is more honest than a flag the shell cannot honour.
#[derive(Model, Debug, Clone)]
#[model(table = "deployments", ordering = "-created_at")]
pub struct Deployment {
    #[field(primary_key)]
    pub id: i64,
    #[field(
        foreign_key = "Application",
        on_delete = "cascade",
        related_name = "deployments"
    )]
    pub app: ForeignKey<Application>,
    pub replicas: i64,
    #[field(max_length = 20)]
    pub status: String,
    /// Free-form rollout settings. `JSONB`, because nothing queries inside it.
    pub config: serde_json::Value,
    #[field(auto_now_add)]
    pub created_at: DateTime<Utc>,
    /// Refreshed on every write, including by a `QuerySet::update` that does not
    /// mention it.
    #[field(auto_now)]
    pub updated_at: DateTime<Utc>,
}

/// A tenant's managed database (DBaaS) — the control plane's **registry** of the
/// instance, not the database itself. The real database is an Addon (a container)
/// the runtime provisions; this row records who owns which, under which plan,
/// with the access-token digest (never the token). This is the first **real**
/// platform table modelled on the ORM — ADR 0072 gate step #1 — taken from the
/// hand-written `tenant_databases` in
/// `delonix-api/src/cpstore/store_base.rs`.
///
/// A product in a catalogue — a **generic** example that exercises the three
/// features the platform's real tables need, without being a platform entity:
/// a non-serial `TEXT` primary key (`sku`), a composite `UNIQUE (category, name)`,
/// and a table-level `CHECK`. Kept here so those features stay covered by the
/// generic example set; the platform's own table that uses them for real
/// (`TenantDatabase`) lives in `delonix-store-models`, over the ORM, per the
/// independence rule.
#[derive(Model, Debug, Clone)]
#[model(
    table = "products",
    unique_together = "category, name",
    check = "price >= 0"
)]
pub struct Product {
    /// The stock-keeping unit — the natural key, so no surrogate `id`. A `TEXT`
    /// PK, caller-supplied, not a serial.
    #[field(primary_key, max_length = 40)]
    pub sku: String,
    #[field(max_length = 60)]
    pub category: String,
    #[field(max_length = 120)]
    pub name: String,
    pub price: Decimal,
}

/// The schema these models expect, as SQL — now **generated** from the same
/// `&'static ModelMeta` the queries read, via `dorm-migrations`.
///
/// It used to be a hand-written stopgap; that hand-written text is now frozen in
/// `tests/schema_parity.rs` as the oracle this generator is proven against. The
/// `TenantDatabase` registry table is deliberately left out here (the typed e2e
/// seeds only the four example models); it has its own parity check in that test.
pub fn schema_sql() -> String {
    use dorm_core::model::Model;
    dorm_migrations::ddl::schema_for(&[
        <Tenant as Model>::meta(),
        <Plan as Model>::meta(),
        <Application as Model>::meta(),
        <Deployment as Model>::meta(),
    ])
    .expect("the example models' schema generates")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dorm_core::prelude::*;
    use dorm_core::registry;

    #[test]
    fn every_model_reaches_the_registry_without_being_listed_anywhere() {
        // This is the property the REPL depends on. Nothing in this crate
        // enumerates the four models; the linker does.
        let mut names: Vec<&str> = registry::models().map(|m| m.name).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["Application", "Deployment", "Plan", "Product", "Tenant"],
            "a model was added or lost without the registry noticing"
        );
    }

    #[test]
    fn the_declared_table_and_ordering_arrive_in_the_metadata() {
        let m = registry::model("Tenant").expect("Tenant is registered");
        assert_eq!(m.table, "tenants");
        assert_eq!(m.ordering, &["-created_at"]);
        assert!(!m.tenant_scoped);

        let a = registry::model("Application").expect("Application is registered");
        assert!(a.tenant_scoped);
    }

    #[test]
    fn a_foreign_key_records_its_target_and_its_related_name() {
        let a = registry::model("Application").expect("Application is registered");
        let f = a.field("tenant").expect("the tenant field exists");
        let r = f.relation.expect("it is a relation");
        assert_eq!(r.model, "Tenant");
        assert_eq!(r.related_name, Some("apps"));
        assert_eq!(r.on_delete, OnDelete::Cascade);
        // The column is named after the field, matching how this repo already
        // spells it in `cpstore` — `tenant`, not `tenant_id`.
        assert_eq!(f.column, "tenant");
    }

    #[test]
    fn the_related_name_resolves_backwards_from_the_target() {
        // What `Count("apps")` on Tenant relies on.
        let (model, fk) =
            registry::reverse_relation("Tenant", "apps").expect("apps points back at Tenant");
        assert_eq!(model.name, "Application");
        assert_eq!(fk.name, "tenant");
        assert!(registry::reverse_relation("Tenant", "nope").is_none());
    }

    #[test]
    fn column_types_are_inferred_from_the_rust_types() {
        let p = registry::model("Plan").expect("Plan is registered");
        assert_eq!(p.field("monthly_price").unwrap().ty, FieldType::Decimal);
        assert_eq!(p.field("code").unwrap().ty, FieldType::Text);

        let d = registry::model("Deployment").expect("Deployment is registered");
        assert_eq!(d.field("config").unwrap().ty, FieldType::Json);
        assert_eq!(d.field("replicas").unwrap().ty, FieldType::I64);
        assert_eq!(d.field("created_at").unwrap().ty, FieldType::Timestamp);
        // A foreign key's column holds the target's key.
        assert_eq!(d.field("app").unwrap().ty, FieldType::I64);
    }

    #[test]
    fn the_new_row_omits_what_the_database_supplies() {
        // `NewTenant` has `slug` and `name` and nothing else: `id` is a serial
        // and `created_at` is an `auto_now_add` stamp.
        let n = NewTenant {
            slug: "acme".into(),
            name: "ACME Lda".into(),
        };
        let mut names: Vec<String> = n.column_values().into_iter().map(|(k, _)| k).collect();
        names.sort();
        assert_eq!(names, ["name", "slug"]);
    }

    #[test]
    fn auto_now_columns_are_left_out_of_an_instance_write() {
        let d = Deployment {
            id: 1,
            app: ForeignKey::from_id(Value::I64(2)),
            replicas: 3,
            status: "running".into(),
            config: serde_json::json!({}),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let names: Vec<String> = d.column_values().into_iter().map(|(k, _)| k).collect();
        // Both stamps are the database's to set.
        assert!(!names.contains(&"created_at".to_string()));
        assert!(!names.contains(&"updated_at".to_string()));
        assert!(names.contains(&"replicas".to_string()));
    }

    #[test]
    fn the_typed_field_handles_build_the_paths_the_repl_would_send() {
        // The acceptance syntax, checked without a database.
        let q = application::tenant.slug().eq("acme");
        let mut paths = Vec::new();
        q.paths(&mut paths);
        assert_eq!(paths, ["tenant__slug"]);

        let q = application::name.icontains("ap");
        let mut paths = Vec::new();
        q.paths(&mut paths);
        assert_eq!(paths, ["name__icontains"]);

        // Two relations deep: Deployment → Application → Tenant.
        let q = deployment::app.tenant().slug().eq("acme");
        let mut paths = Vec::new();
        q.paths(&mut paths);
        assert_eq!(paths, ["app__tenant__slug"]);
    }

    #[test]
    fn a_primary_key_is_reported_and_an_unsaved_instance_has_none() {
        let t = Tenant {
            id: 42,
            slug: "acme".into(),
            name: "ACME".into(),
            created_at: Utc::now(),
        };
        assert_eq!(t.pk(), Value::I64(42));
        assert_eq!(t.as_fk().to_value(), Value::I64(42));
    }
}
