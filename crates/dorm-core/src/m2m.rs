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
use crate::meta::ModelMeta;
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
        Self::from_meta(J::meta(), a_field, b_field)
    }

    /// Build from the junction model's registered *name* and its two foreign-key
    /// field names — the string-keyed counterpart of [`on`](Self::on), for the
    /// dynamic path (the REPL, or a derive-generated accessor) where the junction
    /// type is not named in code. The model must be registered.
    pub fn through(junction: &str, a_field: &str, b_field: &str) -> Result<Self> {
        Self::from_meta(crate::registry::model(junction)?, a_field, b_field)
    }

    fn from_meta(j: &'static ModelMeta, a_field: &str, b_field: &str) -> Result<Self> {
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

    /// Fix the "left" key, so every operation acts on `a` without repeating it.
    /// The shape a derived `#[m2m(..)]` accessor returns — `post.tags()` binds
    /// `post.pk()` here, leaving `add`/`remove`/`clear`/`keys` to take only the
    /// other side.
    pub fn bind(self, a: Value) -> BoundManyToMany {
        BoundManyToMany { link: self, a }
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

/// A [`ManyToMany`] with its "left" key already bound — what a derive-generated
/// accessor hands back, so `post.tags()?.add(tag_pk)` never repeats `post`'s key.
///
/// Every method forwards to the underlying [`ManyToMany`], prefixing the bound
/// `a` value that [`ManyToMany::bind`] (and the `#[m2m(..)]` accessor) captured
/// from the instance's `pk()`. This is the Django related-manager ergonomics:
/// the instance you reached it through *is* the left side.
#[derive(Debug, Clone)]
pub struct BoundManyToMany {
    link: ManyToMany,
    a: Value,
}

impl BoundManyToMany {
    /// Associate the bound instance with `b`. Idempotent — see [`ManyToMany::add`].
    pub async fn add(&self, b: Value) -> Result<()> {
        self.link.add(self.a.clone(), b).await
    }

    /// Drop the association with `b`; returns how many rows went (0 if unlinked).
    pub async fn remove(&self, b: Value) -> Result<u64> {
        self.link.remove(self.a.clone(), b).await
    }

    /// Drop every association the bound instance has; returns how many rows went.
    pub async fn clear(&self) -> Result<u64> {
        self.link.clear(self.a.clone()).await
    }

    /// Whether the bound instance is linked to `b`.
    pub async fn exists(&self, b: Value) -> Result<bool> {
        self.link.exists(self.a.clone(), b).await
    }

    /// How many things the bound instance is linked to.
    pub async fn count(&self) -> Result<i64> {
        self.link.count(self.a.clone()).await
    }

    /// The keys of everything the bound instance is linked to, decoded as `K` —
    /// for a text-keyed junction, say `keys::<String>()`. See [`ManyToMany::keys`].
    pub async fn keys<K: Decode>(&self) -> Result<Vec<K>> {
        self.link.keys::<K>(self.a.clone()).await
    }

    /// The linked keys as `i64` — the common BIGINT-junction case, so a caller
    /// need not name the type. [`keys`](Self::keys) covers any other key type.
    pub async fn all_keys(&self) -> Result<Vec<i64>> {
        self.link.keys::<i64>(self.a.clone()).await
    }

    /// The underlying unbound association and the bound key, for the rare caller
    /// that needs the pair back.
    pub fn into_parts(self) -> (ManyToMany, Value) {
        (self.link, self.a)
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
