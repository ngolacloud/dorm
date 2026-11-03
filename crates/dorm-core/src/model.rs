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
use crate::scope;
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
    // Validate early that the model can be saved by instance; `versioned_update`
    // and `insert_returning` both rely on there being a primary key.
    meta.primary_key().ok_or_else(|| {
        Error::Config(format!(
            "model `{}` declares no primary key, so an instance cannot be saved",
            meta.name
        ))
    })?;

    if obj.pk().is_null() {
        let values = obj.column_values();
        let batch = std::slice::from_ref(&values);
        let sql = scope::compile_insert(meta, batch)?;
        let rows = insert_returning(meta, &sql, batch).await?;
        let row = rows.first().ok_or_else(|| {
            Error::Db("the insert returned no row, so the new key is unknown".into())
        })?;
        *obj = T::from_row(row, 0)?;
        return Ok(());
    }

    // An update by primary key, which needs no subquery and no join. On a
    // versioned model (`#[field(version)]`) the current version — the one read
    // into `obj` — is the expected one.
    let expected = meta
        .version_field()
        .map(|vf| {
            obj.column_values()
                .iter()
                .find(|(n, _)| n == vf.name)
                .and_then(|(_, v)| v.as_i64())
                .ok_or_else(|| {
                    Error::Config(format!(
                        "model `{}` has a version field `{}` that is not an integer value",
                        meta.name, vf.name
                    ))
                })
        })
        .transpose()?;
    versioned_update(obj, expected).await
}

/// The update half of [`save_instance`], shared with [`update_if_version`].
///
/// When `expected` is `Some`, the model is versioned (ADR 0023): the `UPDATE`
/// bumps the version column and guards on `version = expected`, so a row changed
/// under the writer's feet matches nothing and surfaces as [`Error::Conflict`]
/// rather than silently clobbering the other writer. When `None`, it is an
/// ordinary last-write-wins update by primary key.
async fn versioned_update<T: Model>(obj: &mut T, expected: Option<i64>) -> Result<()> {
    let meta = T::meta();
    let pk_field = meta
        .primary_key()
        .ok_or_else(|| Error::Config(format!("model `{}` declares no primary key", meta.name)))?;
    let version = meta.version_field();

    let values = obj.column_values();
    let mut plan = compiler::Plan::new(meta);
    plan.filters
        .push(crate::expr::Q::new(pk_field.name.to_string(), obj.pk()));

    let mut sets: Vec<crate::expr::Assignment> = Vec::new();
    for (name, v) in &values {
        let f = meta.field(name).ok_or_else(|| Error::UnknownField {
            model: meta.name.to_string(),
            field: name.clone(),
        })?;
        // `auto_now_add` is the creation stamp; rewriting it on every save would
        // make it a modification stamp, which is what `auto_now` is for.
        if f.auto_now_add {
            continue;
        }
        // The version column is not written as a literal: the database increments
        // it, so two concurrent bumps cannot land on the same number.
        if version.is_some_and(|vf| vf.name == name) {
            continue;
        }
        sets.push(crate::expr::Assignment::new(
            name.clone(),
            crate::expr::F::Lit(v.clone()),
        ));
    }

    if let (Some(vf), Some(expected)) = (version, expected) {
        // version = version + 1, guarded by version = $expected.
        sets.push(crate::expr::Assignment::new(
            vf.name.to_string(),
            crate::expr::F::col(vf.name) + 1i64,
        ));
        plan.filters.push(crate::expr::Q::new(
            vf.name.to_string(),
            Value::I64(expected),
        ));
        let sql = scope::compile_update(&plan, &sets)?;
        let affected = run_execute(&sql).await?;
        if affected == 0 {
            let actual = current_version(meta, vf, pk_field, obj.pk()).await?;
            return Err(Error::Conflict {
                model: meta.name.to_string(),
                expected,
                actual,
            });
        }
        // Reload so `obj` carries the new version — a second save in the same
        // scope then guards on the right number.
        refresh_instance(obj).await?;
        return Ok(());
    }

    let sql = scope::compile_update(&plan, &sets)?;
    run_execute(&sql).await?;
    Ok(())
}

/// Save `obj` only if its version column still equals `expected`, otherwise
/// [`Error::Conflict`] — the explicit form of the optimistic check that `save`
/// does implicitly, for an `If-Match`/`ETag` request that carries the expected
/// version separately from the in-memory object (ADR 0023).
pub async fn update_if_version<T: Model>(obj: &mut T, expected: i64) -> Result<()> {
    db::guard_write("update")?;
    if T::meta().version_field().is_none() {
        return Err(Error::Config(format!(
            "model `{}` has no `#[field(version)]`, so update_if_version does not apply",
            T::meta().name
        )));
    }
    versioned_update(obj, Some(expected)).await
}

/// Read a row's current version counter, or `None` if the row is gone — the
/// `actual` an [`Error::Conflict`] reports after a lost optimistic write.
async fn current_version(
    meta: &'static ModelMeta,
    version_field: &'static crate::meta::FieldMeta,
    pk_field: &'static crate::meta::FieldMeta,
    pk: Value,
) -> Result<Option<i64>> {
    let mut plan = compiler::Plan::new(meta);
    plan.filters
        .push(crate::expr::Q::new(pk_field.name.to_string(), pk));
    plan.values = vec![version_field.name.to_string()];
    plan.limit = Some(1);
    let sql = scope::compile_select(&plan)?;
    let rows = run_query(&sql).await?;
    match rows.first() {
        None => Ok(None),
        Some(row) => Ok(row
            .try_get::<i64>(0)
            .ok()
            .or_else(|| row.try_get::<i32>(0).ok().map(|n| n as i64))),
    }
}

/// An opaque `ETag` for a row's version, for `If-Match` optimistic concurrency
/// over HTTP. Decodable back to the version with [`version_from_etag`]; a wrong
/// version simply loses the [`update_if_version`] guard, so the token needs no
/// signature (ADR 0023).
pub fn etag(version: i64) -> String {
    format!("\"dorm-v{version}\"")
}

/// Recover the version an [`etag`] encodes, accepting a weak (`W/"..."`) prefix.
pub fn version_from_etag(tag: &str) -> Option<i64> {
    tag.trim()
        .trim_start_matches("W/")
        .trim_matches('"')
        .strip_prefix("dorm-v")?
        .parse()
        .ok()
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
    let sql = scope::compile_delete(&plan)?;
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
    let sql = scope::compile_select(&plan)?;
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
/// ([`FieldMeta::is_database_generated`](crate::meta::FieldMeta::is_database_generated)) the database assigns: `last_insert_id`
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
        let reload = scope::compile_select(&plan)?;
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

#[cfg(test)]
mod tests {
    use super::{etag, version_from_etag};

    #[test]
    fn an_etag_round_trips_to_its_version() {
        let tag = etag(7);
        assert_eq!(version_from_etag(&tag), Some(7));
        // A weak-etag prefix an HTTP stack may add is tolerated.
        assert_eq!(version_from_etag(&format!("W/{tag}")), Some(7));
    }

    #[test]
    fn a_foreign_or_malformed_etag_does_not_parse() {
        assert_eq!(version_from_etag("\"42\""), None);
        assert_eq!(version_from_etag("garbage"), None);
        assert_eq!(version_from_etag("\"dorm-vNaN\""), None);
    }
}
