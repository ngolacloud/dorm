//! `prefetch`: load a reverse relation for many parents in **one** query, so a
//! list-then-children access pattern does not become N+1 (ADR 0013).
//!
//! `select_related` already handles the forward direction — a foreign key — with
//! a JOIN in the same statement. The reverse direction (a parent's *many*
//! children) cannot be a JOIN without multiplying the parent rows, so it is a
//! second query — but **one** second query for the whole batch, not one per
//! parent. That is the difference between a page that issues 2 queries and one
//! that issues 101.
//!
//! The result is not grafted onto the parent instances (DORM's models are plain
//! structs, not identity-mapped entities); it is returned as a [`Prefetched`]
//! lookup you index by parent:
//!
//! ```no_run
//! # async fn f() -> dorm_core::Result<()> {
//! # use dorm_core::prefetch::prefetch;
//! # use dorm_core::model::Model;
//! # #[derive(Clone)] struct Tenant;
//! # #[derive(Clone)] struct Application;
//! # impl Model for Tenant { fn meta() -> &'static dorm_core::meta::ModelMeta { unimplemented!() } fn from_row(_: &dorm_core::row::Row, _: usize) -> dorm_core::Result<Self> { unimplemented!() } fn pk(&self) -> dorm_core::value::Value { unimplemented!() } fn column_values(&self) -> Vec<(String, dorm_core::value::Value)> { unimplemented!() } }
//! # impl Model for Application { fn meta() -> &'static dorm_core::meta::ModelMeta { unimplemented!() } fn from_row(_: &dorm_core::row::Row, _: usize) -> dorm_core::Result<Self> { unimplemented!() } fn pk(&self) -> dorm_core::value::Value { unimplemented!() } fn column_values(&self) -> Vec<(String, dorm_core::value::Value)> { unimplemented!() } }
//! # let tenants: Vec<Tenant> = vec![];
//! let apps = prefetch::<Tenant, Application>(&tenants, "apps").await?; // one query
//! for t in &tenants {
//!     for _app in apps.of(t) { /* … */ }                               // no further queries
//! }
//! # Ok(()) }
//! ```

use std::collections::{HashMap, HashSet};

use crate::error::{Error, Result};
use crate::expr::Q;
use crate::model::Model;
use crate::queryset::Manager;
use crate::registry;
use crate::value::Value;

/// A reverse relation's children, grouped by the parent they belong to.
///
/// Built by [`prefetch`]. Look children up by parent with [`of`](Self::of); a
/// parent with no children gets an empty slice, never a missing-key surprise.
pub struct Prefetched<C> {
    by_parent: HashMap<String, Vec<C>>,
}

impl<C> Prefetched<C> {
    /// The children of `parent` — an empty slice when it has none.
    pub fn of<P: Model>(&self, parent: &P) -> &[C] {
        self.by_parent
            .get(&key(&parent.pk()))
            .map_or(&[], Vec::as_slice)
    }

    /// Total children loaded across every parent.
    pub fn len(&self) -> usize {
        self.by_parent.values().map(Vec::len).sum()
    }

    /// Whether no children were loaded at all.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A stable grouping key for a primary-key value, distinguishing variants so an
/// `I64(5)` and a `Text("5")` never collide. The parent's `pk()` and the child's
/// foreign-key value are the same `Value` variant, so they render identically.
fn key(v: &Value) -> String {
    format!("{v:?}")
}

/// Load the reverse relation `related_name` for `parents` in a single query, and
/// return it grouped by parent.
///
/// `P` is the parent model, `C` the child the relation points to; `related_name`
/// is the name declared on the child's foreign key (`#[field(foreign_key = …,
/// related_name = "apps")]`). The call runs one `SELECT … WHERE fk IN (parent
/// keys)` — a read, so it is replica-eligible like any other — and groups the
/// children by the parent their foreign key names.
///
/// Errors if the relation does not exist, or if `C` is not the model it points
/// to (a mismatch caught by name, not silently wrong).
pub async fn prefetch<P, C>(parents: &[P], related_name: &str) -> Result<Prefetched<C>>
where
    P: Model,
    C: Model,
{
    // Which child model, and which foreign key, points back at `P` under this
    // name. Only the child's metadata records the relation, so this scans the
    // registry (see `registry::reverse_relation`).
    let (child_meta, fk) =
        registry::reverse_relation(P::meta().name, related_name).ok_or_else(|| {
            Error::Config(format!(
                "`{}` has no reverse relation `{related_name}`; set `related_name` on the \
                 foreign key that points at it",
                P::meta().name
            ))
        })?;

    // The child type the caller named must be the one the relation points to —
    // otherwise the returned `Vec<C>` would be decoded from the wrong table.
    if child_meta.name != C::meta().name {
        return Err(Error::Config(format!(
            "reverse relation `{related_name}` on `{}` points to `{}`, not `{}`",
            P::meta().name,
            child_meta.name,
            C::meta().name
        )));
    }

    // The distinct parent keys to fetch children for. Nothing to do with none.
    let mut seen = HashSet::new();
    let pks: Vec<Value> = parents
        .iter()
        .map(Model::pk)
        .filter(|v| seen.insert(key(v)))
        .collect();
    if pks.is_empty() {
        return Ok(Prefetched {
            by_parent: HashMap::new(),
        });
    }

    // The one query: every child whose foreign key is in the parent key set.
    let children: Vec<C> = Manager::<C>::new()
        .filter(Q::many(format!("{}__in", fk.name), pks))
        .all()
        .await?;

    // Group each child under the parent its foreign key names. The foreign key's
    // value is read from the instance's own column values (keyed by field name),
    // which is the parent key it was stored with.
    let mut by_parent: HashMap<String, Vec<C>> = HashMap::new();
    for child in children {
        let fk_value = child
            .column_values()
            .into_iter()
            .find(|(name, _)| name == fk.name)
            .map(|(_, v)| v)
            .ok_or_else(|| {
                Error::Db(format!(
                    "child `{}` did not expose its foreign key `{}`",
                    child_meta.name, fk.name
                ))
            })?;
        by_parent.entry(key(&fk_value)).or_default().push(child);
    }

    Ok(Prefetched { by_parent })
}
