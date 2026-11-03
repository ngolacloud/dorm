//! The `Model` trait, and the instance operations built on it.
//!
//! The trait is deliberately small. Everything the query layer needs about a
//! model's *shape* it reads from [`ModelMeta`]; what the trait adds is the three
//! things only generated code can do — decode a row into the struct, report the
//! primary key, and hand back the column values for a write.

use crate::compiler;
use crate::db;
use crate::error::{Error, Result};
use crate::meta::ModelMeta;
use crate::queryset::Manager;
use crate::row::Row;
use crate::value::Value;

/// A type that maps to one table.
///
/// Implemented by `#[derive(Model)]`; writing it by hand is possible but means
/// keeping the metadata and the decoder in step, which is the coupling the macro
/// exists to remove.
pub trait Model: Sized + Send + Sync + 'static {
    /// The metadata the derive macro also submitted to the registry.
    fn meta() -> &'static ModelMeta;

    /// Decode one row, reading this model's columns starting at `offset`.
    ///
    /// The offset is what makes `select_related` work: the root model decodes at
    /// 0 and each joined model decodes further along the same row.
    fn from_row(row: &Row, offset: usize) -> Result<Self>;

    /// The primary key, or [`Value::Null`] for an instance not yet saved.
    fn pk(&self) -> Value;

    /// The column values for a write, database-generated columns excluded.
    fn column_values(&self) -> Vec<(String, Value)>;

    /// Attach a row loaded by `select_related` to the named foreign key.
    fn attach_related(&mut self, _field: &str, _row: &Row, _offset: usize) -> Result<()> {
        Ok(())
    }

    /// The entry point: `Tenant::objects()`.
    fn objects() -> Manager<Self> {
        Manager::new()
    }
}

/// A struct describing a row to insert.
///
/// `create` takes one of these rather than a `Model`, so that a column the
/// database fills in — a serial primary key, an `auto_now_add` timestamp — is
/// not something the caller has to invent a value for. `#[derive(Model)]`
/// generates `New<Model>` and this impl.
pub trait NewRow<T: Model> {
    /// Column values in metadata order, as `(field name, value)`.
    fn column_values(&self) -> Vec<(String, Value)>;
}

/// Insert or update an instance, depending on whether it has a primary key.
pub async fn save_instance<T: Model>(obj: &mut T) -> Result<()> {
    db::guard_write("save")?;
    let meta = T::meta();
    let pk_field = meta.primary_key().ok_or_else(|| {
        Error::Config(format!(
            "model `{}` declares no primary key, so an instance cannot be saved",
            meta.name
        ))
    })?;

    if obj.pk().is_null() {
        let values = obj.column_values();
        let batch = std::slice::from_ref(&values);
        let sql = compiler::compile_insert_on(meta, batch, db::dialect())?;
        let rows = insert_returning(meta, &sql, batch).await?;
        let row = rows.first().ok_or_else(|| {
            Error::Db("the insert returned no row, so the new key is unknown".into())
        })?;
        *obj = T::from_row(row, 0)?;
        return Ok(());
    }

    // An update by primary key, which needs no subquery and no join.
    let values = obj.column_values();
    let mut assignments = Vec::new();
    for (name, _) in &values {
        let f = meta.field(name).ok_or_else(|| Error::UnknownField {
            model: meta.name.to_string(),
            field: name.clone(),
        })?;
        // `auto_now_add` is the creation stamp; rewriting it on every save would
        // make it a modification stamp, which is what `auto_now` is for.
        if f.auto_now_add {
            continue;
        }
        assignments.push(name.clone());
    }

    let mut plan = compiler::Plan::new(meta);
    plan.filters
        .push(crate::expr::Q::new(pk_field.name.to_string(), obj.pk()));
    let sets: Vec<crate::expr::Assignment> = assignments
        .iter()
        .map(|name| {
            let v = values
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
                .unwrap_or(Value::Null);
            crate::expr::Assignment::new(name.clone(), crate::expr::F::Lit(v))
        })
        .collect();
    let sql = compiler::compile_update(&plan, &sets)?;
    run_execute(&sql).await?;
    Ok(())
}

/// Delete this instance by primary key.
pub async fn delete_instance<T: Model>(obj: &T) -> Result<u64> {
    db::guard_write("delete")?;
    let meta = T::meta();
    let pk_field = meta.primary_key().ok_or_else(|| {
        Error::Config(format!(
            "model `{}` declares no primary key, so an instance cannot be deleted",
            meta.name
        ))
    })?;
    if obj.pk().is_null() {
        return Err(Error::Config(
            "this instance has never been saved, so there is nothing to delete".into(),
        ));
    }
    let mut plan = compiler::Plan::new(meta);
    plan.filters
        .push(crate::expr::Q::new(pk_field.name.to_string(), obj.pk()));
    let sql = compiler::compile_delete(&plan)?;
    run_execute(&sql).await
}

/// Re-read this instance from the database, discarding local changes.
pub async fn refresh_instance<T: Model>(obj: &mut T) -> Result<()> {
    let meta = T::meta();
    let pk_field = meta
        .primary_key()
        .ok_or_else(|| Error::Config(format!("model `{}` declares no primary key", meta.name)))?;
    let mut plan = compiler::Plan::new(meta);
    plan.filters
        .push(crate::expr::Q::new(pk_field.name.to_string(), obj.pk()));
    plan.limit = Some(1);
    let sql = compiler::compile_select(&plan)?;
    let rows = run_query(&sql).await?;
    let row = rows.first().ok_or_else(|| Error::DoesNotExist {
        model: meta.name.to_string(),
    })?;
    *obj = T::from_row(row, 0)?;
    Ok(())
}

/// Run a compiled `INSERT` and hand back the written rows, database-generated
/// columns (the serial key, `auto_now_add` stamps) filled in — the same result
/// shape on every backend, whether or not it has `RETURNING`.
///
/// On PostgreSQL/SQLite the compiled statement is already `INSERT … RETURNING`,
/// so one [`run_query`] reads the rows back. MySQL has no `RETURNING` (ADR 0018):
/// the statement is a bare `INSERT`, so this runs it, works out the keys of the
/// rows it wrote, and reloads them by primary key. Both steps run inside one
/// [`db::atomic`] so they share a connection — `last_insert_id` is per-connection,
/// and the reload must see the uncommitted rows.
///
/// How the keys are known depends on who supplies them. A serial/identity key
/// ([`FieldMeta::is_database_generated`]) the database assigns: `last_insert_id`
/// is the first and a single `INSERT` lays the block down contiguously, so the
/// keys are `first .. first + affected`. A caller-supplied key — a `TEXT`
/// primary key like the DBaaS `tenant_databases.addon_name` — is already in the
/// `rows` the caller handed us, so those are the keys to reload by;
/// `last_insert_id` is `0` for such an insert and must not be read as a key
/// (ADR 0018 Inc. 2b follow-up). `rows` is the same column-value list that
/// compiled `sql`, in the same order.
pub async fn insert_returning(
    meta: &'static ModelMeta,
    sql: &compiler::Sql,
    rows: &[Vec<(String, Value)>],
) -> Result<Vec<Row>> {
    if db::dialect().supports(compiler::Capability::Returning) {
        return run_query(sql).await;
    }

    let pk = meta.primary_key().ok_or_else(|| {
        Error::Config(format!(
            "model `{}` has no primary key, so an insert cannot be read back without RETURNING",
            meta.name
        ))
    })?;

    db::atomic(|_tx| async move {
        let (affected, first) = db::insert(&sql.text, &sql.params).await?;
        if affected == 0 {
            return Ok(Vec::new());
        }
        // The keys of the rows just written, to reload them by.
        let ids: Vec<Value> = if pk.is_database_generated() {
            // Serial/identity: the database assigned them. `last_insert_id` is
            // the first; a single `INSERT` assigns the block contiguously.
            (first..first + affected)
                .map(|id| match pk.ty {
                    crate::value::FieldType::I32 => Value::I32(id as i32),
                    _ => Value::I64(id as i64),
                })
                .collect()
        } else {
            // Caller-supplied (a `TEXT` key, say): the keys are the ones the
            // caller put in `rows`. `last_insert_id` is `0` here, so the serial
            // block above would reload the wrong rows.
            rows.iter()
                .map(|row| {
                    row.iter()
                        .find(|(name, _)| name == pk.name)
                        .map(|(_, v)| v.clone())
                        .ok_or_else(|| {
                            Error::Config(format!(
                                "model `{}` has a caller-supplied primary key `{}`, but the \
                                 inserted row did not set it, so it cannot be read back",
                                meta.name, pk.name
                            ))
                        })
                })
                .collect::<Result<Vec<_>>>()?
        };
        let mut plan = compiler::Plan::new(meta);
        plan.filters
            .push(crate::expr::Q::many(format!("{}__in", pk.name), ids));
        plan.order = vec![crate::expr::OrderBy {
            path: pk.name.to_string(),
            descending: false,
        }];
        let reload = compiler::compile_select_on(&plan, db::dialect())?;
        // On the primary, on the transaction's pinned connection — not a replica.
        db::fetch(&reload.text, &reload.params).await
    })
    .await
}

/// Run compiled SQL that returns rows, against whichever backend the DSN names.
///
/// Routed by what the compiler classified the statement as: a
/// [`Read`](compiler::StatementKind::Read) may go to a read replica
/// ([`db::fetch_read`]), while a [`Write`](compiler::StatementKind::Write) that
/// returns rows — an `INSERT … RETURNING` — stays on the primary ([`db::fetch`]).
pub async fn run_query(sql: &compiler::Sql) -> Result<Vec<Row>> {
    match sql.kind {
        compiler::StatementKind::Read => db::fetch_read(&sql.text, &sql.params).await,
        compiler::StatementKind::Write => db::fetch(&sql.text, &sql.params).await,
    }
}

/// Run compiled SQL for its row count, against whichever backend the DSN names.
pub async fn run_execute(sql: &compiler::Sql) -> Result<u64> {
    db::run(&sql.text, &sql.params).await
}
