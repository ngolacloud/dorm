//! `dorm-migrations` — the schema side of the ORM.
//!
//! The typed API and the REPL turn `&'static ModelMeta` into *queries*
//! (`dorm-core::compiler`). This crate turns the *same* metadata into
//! *schema*: `CREATE TABLE`, indexes and foreign keys, and a versioned,
//! advisory-locked applier.
//!
//! It is the piece [ADR 0072](../../docs/adr/0072-arquitectura-alvo-de-crates-e-o-orm-como-camada-de-persistencia.md)
//! presupposes and that `dorm-models` has always promised in a comment
//! ("The autodetector in `dorm-migrations` will generate the equivalent
//! from the same metadata"). Until now it did not exist.
//!
//! # What it is
//!
//! * [`ddl`] generates the **forward/initial** schema from a set of models —
//!   enough to create a database, and enough to *adopt* one that already has the
//!   tables (every statement is `IF NOT EXISTS`). This is the `expand` step of
//!   `expand → migrate → contract`.
//! * [`autodetect`](fn@autodetect) diffs a stored [`Snapshot`] of the schema's previous state
//!   against the current models and emits the **next versioned migration** —
//!   `CREATE`/`DROP TABLE`, `ADD`/`DROP COLUMN`, `CREATE`/`DROP INDEX` — the
//!   shape of Django's `makemigrations` and Prisma's `migrate dev`. An in-place
//!   column alteration or a composite-constraint change is an explicit
//!   [`Error::UnsupportedChange`] (the table-rebuild path is the next increment),
//!   never a silent wrong result.
//!
//! # The one invariant it keeps
//!
//! No identifier comes from a caller. Table and column names come only from
//! `&'static ModelMeta` built by `#[derive(Model)]`; a foreign key's target is
//! resolved through [`dorm_core::registry`], never spelled by hand. That
//! is the same property that makes the query side safe, carried to the schema.

#![forbid(unsafe_code)]

pub mod autodetect;
pub mod ddl;
pub mod history;
pub mod migrator;

/// What can go wrong building or applying a migration.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A foreign key names a model that is not in the registry. Either the model
    /// crate was not linked, or the `foreign_key = "..."` attribute is misspelt.
    #[error("foreign key points at model `{0}`, which is not registered")]
    UnknownRelationTarget(&'static str),
    /// A foreign key points at a model that has no primary key to reference.
    #[error("model `{0}` has no primary key, so a foreign key cannot reference it")]
    NoPrimaryKey(&'static str),
    /// An error from the ORM core (the database, the pool, a transaction).
    #[error(transparent)]
    Orm(#[from] dorm_core::Error),
    /// The autodetector found a change it does not emit SQL for in this version:
    /// an in-place column alteration (type/nullability/uniqueness/default/foreign
    /// key changed), or a change to a table's composite `UNIQUE`/`CHECK`. The
    /// message names the table and what changed; the next version handles these.
    #[error("unsupported schema change: {0}")]
    UnsupportedChange(String),
    /// A problem reading or writing the on-disk migration history: a directory
    /// that cannot be read, a file that is not valid JSON, or a backend that does
    /// not match the migrations already there. The message says which.
    #[error("migration history: {0}")]
    History(String),
}

pub type Result<T> = std::result::Result<T, Error>;

pub use autodetect::{autodetect, make_migration, Snapshot};
pub use history::{History, StoredMigration};
pub use migrator::{diff, initial, migrate, sync, Migration};
