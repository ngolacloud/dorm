//! Tenant scoping, imposed where the two front ends meet the compiler (ADR 0021).
//!
//! The compiler stays pure: it never reads the ambient tenant, because that would
//! make the same `Plan` compile to different SQL depending on task-local state and
//! break the snapshot contract. Instead this module is the seam *in front of* the
//! compiler — it takes the `Plan` the typed API or the REPL built, and for a
//! `tenant_scoped` model folds in the tenant predicate (reads/updates/deletes) or
//! the tenant column value (inserts) before handing a plain `Plan` to the pure
//! compiler. Every execution path in `dorm-core` compiles through these helpers,
//! so no surface built on them can forget the filter.
//!
//! Fail-closed: a `tenant_scoped` model compiled with no tenant in scope, and not
//! under [`db::with_system_scope`](crate::db::with_system_scope), is
//! [`Error::TenantScopeMissing`] — refusing beats silently reading every tenant.

use crate::compiler::{self, Plan, Sql};
use crate::db;
use crate::error::{Error, Result};
use crate::expr::{Aggregate, Assignment, Q};
use crate::meta::ModelMeta;
use crate::value::Value;

/// The tenant value to constrain a `tenant_scoped` model to, or `None` when no
/// scoping applies (not tenant-scoped, or an audited system scope). Errors
/// fail-closed when a tenant is required but absent.
fn active_tenant(meta: &'static ModelMeta) -> Result<Option<Value>> {
    if !meta.tenant_scoped || db::is_system_scope() {
        return Ok(None);
    }
    // A model that declares `tenant_scoped` but exposes no tenant field is a
    // model bug, not a missing-context one — name it as such.
    if meta.tenant_field().is_none() {
        return Err(Error::Config(format!(
            "model `{}` is tenant_scoped but declares no tenant field",
            meta.name
        )));
    }
    db::current_tenant()
        .map(Some)
        .ok_or(Error::TenantScopeMissing(meta.name))
}

/// A plan with the tenant predicate folded in when the model is scoped, else a
/// clone of the plan untouched. Owned (not `Cow`) so it drops straight into the
/// `compiler::compile_*_on(&plan, …)` call sites without a deref dance; a plan is
/// compiled once per query, so the clone is not on any hot loop.
fn scoped_plan(plan: &Plan) -> Result<Plan> {
    let mut p = plan.clone();
    if let Some(tenant) = active_tenant(plan.model)? {
        let tf = plan
            .model
            .tenant_field()
            .expect("active_tenant checked the field exists");
        p.filters.push(Q::new(tf.name.to_string(), tenant));
    }
    // Soft delete (ADR 0033): hide `deleted_at IS NOT NULL` rows by default, unless
    // the task opted in with `with_deleted`.
    if !db::include_deleted() {
        if let Some(sd) = plan.model.soft_delete_field() {
            p.filters.push(Q::new(format!("{}__isnull", sd.name), true));
        }
    }
    Ok(p)
}

/// Force the tenant column onto every row of an insert for a scoped model, so an
/// insert cannot land a row under another tenant (or under none).
fn scoped_rows(
    meta: &'static ModelMeta,
    rows: &[Vec<(String, Value)>],
) -> Result<Vec<Vec<(String, Value)>>> {
    let Some(tenant) = active_tenant(meta)? else {
        return Ok(rows.to_vec());
    };
    let tf = meta
        .tenant_field()
        .expect("active_tenant checked the field exists");
    let mut out = rows.to_vec();
    for row in &mut out {
        match row.iter_mut().find(|(n, _)| n == tf.name) {
            Some((_, v)) => *v = tenant.clone(),
            None => row.push((tf.name.to_string(), tenant.clone())),
        }
    }
    Ok(out)
}

// The scoped compile surface. Each mirrors a `compiler::compile_*_on`, applies
// the tenant transform, and targets the active backend's dialect.

pub fn compile_select(plan: &Plan) -> Result<Sql> {
    compiler::compile_select_on(&scoped_plan(plan)?, db::dialect())
}

pub fn compile_count(plan: &Plan) -> Result<Sql> {
    compiler::compile_count_on(&scoped_plan(plan)?, db::dialect())
}

pub fn compile_exists(plan: &Plan) -> Result<Sql> {
    compiler::compile_exists_on(&scoped_plan(plan)?, db::dialect())
}

pub fn compile_aggregate(plan: &Plan, aggregates: &[Aggregate]) -> Result<Sql> {
    compiler::compile_aggregate_on(&scoped_plan(plan)?, aggregates, db::dialect())
}

pub fn compile_update(plan: &Plan, assignments: &[Assignment]) -> Result<Sql> {
    compiler::compile_update_on(&scoped_plan(plan)?, assignments, db::dialect())
}

pub fn compile_delete(plan: &Plan) -> Result<Sql> {
    let scoped = scoped_plan(plan)?;
    // Soft delete (ADR 0033): a `delete()` on a soft-delete model stamps
    // `deleted_at` instead of removing the row — unless the task asked for a hard
    // delete (the purge path). The scoped plan already carries `deleted_at IS
    // NULL`, so only live rows are stamped.
    if let Some(sd) = plan.model.soft_delete_field() {
        if !db::is_hard_delete() {
            let now = crate::value::Value::Timestamp(chrono::Utc::now());
            let sets = [Assignment::new(
                sd.name.to_string(),
                crate::expr::F::Lit(now),
            )];
            return compiler::compile_update_on(&scoped, &sets, db::dialect());
        }
    }
    compiler::compile_delete_on(&scoped, db::dialect())
}

pub fn compile_insert(meta: &'static ModelMeta, rows: &[Vec<(String, Value)>]) -> Result<Sql> {
    let rows = scoped_rows(meta, rows)?;
    compiler::compile_insert_on(meta, &rows, db::dialect())
}

/// Hard-delete the soft-deleted rows of `meta` whose `deleted_at` is older than
/// `retain_secs` — the retention purge for a soft-delete model (ADR 0033).
/// Returns how many rows were removed. Run on a schedule. The table and column
/// are `&'static` identifiers from the model's metadata, never caller input.
pub async fn purge_soft_deleted(meta: &'static ModelMeta, retain_secs: i64) -> Result<u64> {
    let sd = meta
        .soft_delete_field()
        .ok_or_else(|| Error::Config(format!("model `{}` is not soft-delete", meta.name)))?;
    db::run(
        &format!(
            "DELETE FROM \"{}\" WHERE \"{}\" IS NOT NULL \
             AND \"{}\" < now() - ($1 || ' seconds')::interval",
            meta.table, sd.column, sd.column
        ),
        &[Value::Text(retain_secs.max(0).to_string())],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{FieldMeta, ModelMeta};
    use crate::value::FieldType;

    const TFIELDS: &[FieldMeta] = &[
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
            name: "tenant",
            column: "tenant",
            ty: FieldType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            index: true,
            max_length: Some(63),
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        },
    ];

    const SCOPED: ModelMeta = ModelMeta {
        name: "ScopedThing",
        table: "scoped_things",
        fields: TFIELDS,
        ordering: &[],
        tenant_scoped: true,
        unique_together: &[],
        checks: &[],
    };

    #[tokio::test]
    async fn a_scoped_model_without_a_tenant_in_scope_is_refused() {
        let plan = Plan::new(&SCOPED);
        match scoped_plan(&plan) {
            Err(Error::TenantScopeMissing(name)) => assert_eq!(name, "ScopedThing"),
            other => panic!("expected TenantScopeMissing, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn with_tenant_injects_the_predicate() {
        db::with_tenant("acme", async {
            let plan = Plan::new(&SCOPED);
            let scoped = scoped_plan(&plan).expect("scopes");
            // The tenant predicate was appended to the filters.
            assert_eq!(scoped.filters.len(), 1);
            let mut paths = Vec::new();
            scoped.filters[0].paths(&mut paths);
            assert_eq!(paths, ["tenant"]);
        })
        .await;
    }

    #[tokio::test]
    async fn system_scope_bypasses_the_predicate() {
        db::with_system_scope(async {
            let plan = Plan::new(&SCOPED);
            let scoped = scoped_plan(&plan).expect("no scoping under system scope");
            assert!(scoped.filters.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn an_insert_row_gets_the_tenant_forced_onto_it() {
        db::with_tenant("acme", async {
            // A row that tries to set a *different* tenant is overridden.
            let rows = vec![vec![
                ("id".to_string(), Value::I64(1)),
                ("tenant".to_string(), Value::Text("evil".into())),
            ]];
            let scoped = scoped_rows(&SCOPED, &rows).expect("scopes");
            let tenant = scoped[0]
                .iter()
                .find(|(n, _)| n == "tenant")
                .map(|(_, v)| v.clone());
            assert_eq!(tenant, Some(Value::Text("acme".into())));
        })
        .await;
    }
}
