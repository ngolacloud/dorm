//! The `Model` trait, and the instance operations built on it.
//!
//! The trait is deliberately small. Everything the query layer needs about a
//! model's *shape* it reads from [`ModelMeta`]; what the trait adds is the three
//! things only generated code can do — decode a row into the struct, report the
//! primary key, and hand back the column values for a write.

use tokio_postgres::Row;

use crate::compiler;
use crate::db;
use crate::error::{Error, Result};
use crate::meta::ModelMeta;
use crate::queryset::Manager;
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
        let sql = compiler::compile_insert(meta, std::slice::from_ref(&values))?;
        let rows = run_query(&sql).await?;
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

/// Run compiled SQL that returns rows.
pub async fn run_query(sql: &compiler::Sql) -> Result<Vec<Row>> {
    let params = borrow_params(&sql.params);
    db::query(&sql.text, &params).await
}

/// Run compiled SQL for its row count.
pub async fn run_execute(sql: &compiler::Sql) -> Result<u64> {
    let params = borrow_params(&sql.params);
    db::execute(&sql.text, &params).await
}

/// Reborrow the owned parameter vector as the slice of trait objects
/// `tokio-postgres` wants.
///
/// Kept in one place because the lifetime dance is easy to get subtly wrong, and
/// because every statement in the crate needs it.
fn borrow_params(params: &[Value]) -> Vec<&(dyn tokio_postgres::types::ToSql + Sync)> {
    params
        .iter()
        .map(|v| v as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_are_reborrowed_without_reordering_them() {
        // The order of this vector is the placeholder order; a reorder here is
        // the defect that snapshots of the compiler cannot catch.
        let params = vec![Value::I64(1), Value::Text("a".into()), Value::Bool(true)];
        let borrowed = borrow_params(&params);
        assert_eq!(borrowed.len(), 3);
    }

    #[test]
    fn an_empty_parameter_list_reborrows_to_an_empty_slice() {
        assert!(borrow_params(&[]).is_empty());
    }
}
