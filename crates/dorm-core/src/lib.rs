//! `dorm-core` — a Django-shaped ORM for PostgreSQL, built on
//! `tokio-postgres`.
//!
//! # The shape of it
//!
//! ```text
//!   #[derive(Model)]  ──submits──▶  registry (a linker-built slice)
//!         │                              │
//!    typed handles                  model metadata
//!         │                              │
//!         ▼                              ▼
//!      Q / F  ──────────▶  Plan  ──────────▶  compiler  ──▶  Sql { text, params }
//!                            ▲                                    │
//!                     the REPL builds                             ▼
//!                     the same Plan                      db::query / db::execute
//! ```
//!
//! Two front ends, one compiler. The typed API builds a [`compiler::Plan`] out of
//! generated field handles; the REPL builds the same `Plan` out of strings it
//! resolves through the [`registry`]. Neither can name a table or a column that
//! the derive macro did not put in the binary, which is what makes it safe to
//! hand the dynamic half to an operator.
//!
//! # Where the invariants live
//!
//! * **No identifier comes from a caller.** [`registry::resolve`] turns a filter
//!   key into a `&'static FieldMeta` or an error, and the compiler quotes only
//!   those static strings.
//! * **No value is ever formatted into SQL.** Every value becomes a `$n`
//!   placeholder; see [`value::Value`]'s `ToSql`.
//! * **The compiler does no I/O**, so it is tested with snapshots that pin the
//!   generated text *and* the parameter order.

// The ORM has no business doing anything unsafe, and saying so here means a
// future contributor cannot add some without deleting this line.
#![forbid(unsafe_code)]

pub mod compiler;
pub mod db;
pub mod error;
pub mod expr;
pub mod field;
pub mod lookup;
pub mod m2m;
pub mod meta;
pub mod model;
pub mod queryset;
pub mod registry;
pub mod row;
pub mod value;

pub use error::{Error, Result};

// Re-exported so `#[derive(Model)]` can name them without the model's crate
// having to depend on them directly. Generated code refers to these paths, which
// is what keeps a model crate's manifest down to this one dependency.
pub use inventory;

/// Types the generated code names, which callers have no reason to.
///
/// Re-exported here so a crate that only defines models needs `dorm-core`
/// as its single dependency — without this, every such crate would have to carry
/// a `tokio-postgres` entry it never mentions itself.
#[doc(hidden)]
pub mod __private {
    pub use crate::row::Row;
}

/// Everything a model definition and a query need.
///
/// ```ignore
/// use dorm_core::prelude::*;
/// ```
pub mod prelude {
    pub use crate::db::{self, atomic, Tx};
    pub use crate::error::{Error, Result};
    pub use crate::expr::{AggKind, Aggregate, Assignment, OrderBy, F, Q};
    pub use crate::field::{Field, ForeignKey};
    pub use crate::m2m::{BoundManyToMany, ManyToMany};
    pub use crate::meta::{FieldMeta, ModelMeta, OnDelete, Relation};
    pub use crate::model::{Model, NewRow};
    pub use crate::queryset::{Manager, QuerySet, ValueRow};
    pub use crate::value::{ColumnType, FieldType, ToValue, Value};
}
