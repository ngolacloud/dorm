//! Many-to-many associations over an explicit junction table.
//!
//! DORM has no `ManyToMany` field type of its own; a many-to-many is modelled the
//! way Django's `through=` and an explicit join table always allow — a small
//! model with two foreign keys.
//!
//! The *query* side already works with nothing extra: the registry's path
//! resolver joins across relations in both directions, so from a junction model
//! `Authorship { author, book }` a filter like
//! `Book::objects().filter_str("authorships__author", author_pk)` reaches an
//! author's books through the junction, and `filter_str("authorships__book", …)`
//! reaches a book's authors (see the `m2m_e2e` test). That is Django's
//! `filter(books__…)` with no new machinery.
//!
//! What this type adds is the *write* ergonomics Django's related manager gives —
//! `add`, `remove`, `clear`, `exists`, `count`, `keys` — so a caller does not
//! hand-write the junction's INSERT/DELETE each time. It works on the junction
//! table alone, by its two foreign-key columns, so it is backend-agnostic and
//! needs no database to construct.

use crate::db::{self, Backend};
use crate::model::Model;
use crate::row::Decode;
use crate::value::Value;
use crate::{Error, Result};

/// A many-to-many association, read off a junction model's two foreign keys.
#[derive(Debug, Clone, Copy)]
pub struct ManyToMany {
    table: &'static str,
    a_column: &'static str,
    b_column: &'static str,
}

impl ManyToMany {
    /// Build from the junction model `J` and the names of its two foreign-key
    /// *fields* — the one pointing at the "left" model and the one pointing at the
    /// "right" model. The columns are read from `J`'s metadata, so a misspelt
    /// field name is a configuration error, not a malformed query.
    pub fn on<J: Model>(a_field: &str, b_field: &str) -> Result<Self> {
        let j = J::meta();
        let col = |name: &str| {
            j.field(name).map(|f| f.column).ok_or_else(|| {
                Error::Config(format!(
                    "model `{}` has no field `{name}` to use as a many-to-many key",
                    j.name
                ))
            })
        };
        Ok(ManyToMany {
            table: j.table,
            a_column: col(a_field)?,
            b_column: col(b_field)?,
        })
    }

    /// Associate `a` with `b`. Idempotent, like Django's `.add()`: a pair that is
    /// already linked is left alone rather than inserted twice.
    pub async fn add(&self, a: Value, b: Value) -> Result<()> {
        if self.exists(a.clone(), b.clone()).await? {
            return Ok(());
        }
        db::run(
            &format!(
                "INSERT INTO {} ({}, {}) VALUES ({}, {})",
                self.table,
                self.a_column,
                self.b_column,
                ph(1),
                ph(2)
            ),
            &[a, b],
        )
        .await?;
        Ok(())
    }

    /// Drop the association between `a` and `b`. Returns how many rows went — 0 if
    /// they were not linked.
    pub async fn remove(&self, a: Value, b: Value) -> Result<u64> {
        db::run(
            &format!(
                "DELETE FROM {} WHERE {} = {} AND {} = {}",
                self.table,
                self.a_column,
                ph(1),
                self.b_column,
                ph(2)
            ),
            &[a, b],
        )
        .await
    }

    /// Drop every association `a` has. Returns how many rows went.
    pub async fn clear(&self, a: Value) -> Result<u64> {
        db::run(
            &format!(
                "DELETE FROM {} WHERE {} = {}",
                self.table,
                self.a_column,
                ph(1)
            ),
            &[a],
        )
        .await
    }

    /// Whether `a` and `b` are linked.
    pub async fn exists(&self, a: Value, b: Value) -> Result<bool> {
        let rows = db::fetch(
            &format!(
                "SELECT 1 FROM {} WHERE {} = {} AND {} = {} LIMIT 1",
                self.table,
                self.a_column,
                ph(1),
                self.b_column,
                ph(2)
            ),
            &[a, b],
        )
        .await?;
        Ok(!rows.is_empty())
    }

    /// How many things `a` is linked to.
    pub async fn count(&self, a: Value) -> Result<i64> {
        let rows = db::fetch(
            &format!(
                "SELECT count(*) FROM {} WHERE {} = {}",
                self.table,
                self.a_column,
                ph(1)
            ),
            &[a],
        )
        .await?;
        match rows.first() {
            Some(r) => r.try_get::<i64>(0),
            None => Ok(0),
        }
    }

    /// The keys of everything `a` is linked to, decoded as `K` (usually `i64`) —
    /// hand these to `B::objects().filter_str("pk__in", …)` to load the rows, or
    /// use the reverse-relation filter in the module docs.
    pub async fn keys<K: Decode>(&self, a: Value) -> Result<Vec<K>> {
        db::fetch(
            &format!(
                "SELECT {} FROM {} WHERE {} = {}",
                self.b_column,
                self.table,
                self.a_column,
                ph(1)
            ),
            &[a],
        )
        .await?
        .iter()
        .map(|r| r.try_get::<K>(0))
        .collect()
    }
}

/// A placeholder for bind parameter `n` in the configured backend's dialect:
/// `$n` on PostgreSQL, `?n` on SQLite.
fn ph(n: usize) -> String {
    match db::dsn()
        .map(|d| db::backend_of(&d))
        .unwrap_or(Backend::Postgres)
    {
        Backend::Postgres => format!("${n}"),
        Backend::Sqlite => format!("?{n}"),
    }
}
