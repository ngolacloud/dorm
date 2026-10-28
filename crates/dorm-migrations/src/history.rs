//! The on-disk migration history — what `dorm migrate make` writes and
//! `dorm migrate deploy` reads.
//!
//! This is the file layer over the [`autodetect`](crate::autodetect)or: Django
//! keeps migrations as `.py` files and reconstructs the state from them; Prisma
//! keeps a folder per migration with a `migration.sql`. DORM keeps a directory
//! of one JSON file per migration, each recording the version, the name, the
//! backend it was generated for, the statements to run, and — the key part — the
//! [`Snapshot`] of the schema *after* it. That stored snapshot is what the next
//! `make` diffs against, so the history is self-contained: no database is needed
//! to author the next migration, only the directory and the models.
//!
//! A directory is locked to one backend: the statements are dialect-specific, so
//! a `postgres` history and a `sqlite` history are different directories, exactly
//! as a Prisma migrations folder is tied to its provider.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::autodetect::{make_migration, Snapshot};
use crate::ddl::Backend;
use crate::migrator::Migration;
use crate::{Error, Result};
use dorm_core::meta::ModelMeta;

/// One migration as it lives on disk: everything [`migrate`](crate::migrate)
/// needs to apply it, plus the snapshot the next `make` diffs against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMigration {
    pub version: i64,
    pub name: String,
    /// `"postgres"` or `"sqlite"` — the dialect `statements` were generated for.
    pub backend: String,
    pub statements: Vec<String>,
    /// The schema state this migration leaves behind.
    pub snapshot: Snapshot,
}

impl StoredMigration {
    /// The runnable migration, dropping the bookkeeping the applier does not need.
    pub fn to_migration(&self) -> Migration {
        Migration {
            version: self.version,
            name: self.name.clone(),
            statements: self.statements.clone(),
        }
    }
}

/// A migrations directory. Cheap to construct; it touches the filesystem only
/// when a method is called.
pub struct History {
    dir: PathBuf,
}

impl History {
    /// Point at a migrations directory. It need not exist yet — [`make`](Self::make)
    /// creates it on first write, and [`load`](Self::load) treats a missing
    /// directory as an empty history.
    pub fn open(dir: impl AsRef<Path>) -> Self {
        History {
            dir: dir.as_ref().to_path_buf(),
        }
    }

    /// Every migration on disk, ordered by version. A missing directory is an
    /// empty history, not an error.
    pub fn load(&self) -> Result<Vec<StoredMigration>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let entries = std::fs::read_dir(&self.dir)
            .map_err(|e| Error::History(format!("cannot read {}: {e}", self.dir.display())))?;

        let mut out = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|e| Error::History(format!("cannot read an entry: {e}")))?
                .path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .map_err(|e| Error::History(format!("cannot read {}: {e}", path.display())))?;
            let m: StoredMigration = serde_json::from_str(&text)
                .map_err(|e| Error::History(format!("{} is not valid: {e}", path.display())))?;
            out.push(m);
        }

        out.sort_by_key(|m| m.version);
        if let Some(dup) = first_duplicate(&out) {
            return Err(Error::History(format!(
                "two migrations share version {dup} in {}",
                self.dir.display()
            )));
        }
        Ok(out)
    }

    /// The runnable chain, for [`migrate`](crate::migrate) to deploy.
    pub fn to_migrations(&self) -> Result<Vec<Migration>> {
        Ok(self
            .load()?
            .iter()
            .map(StoredMigration::to_migration)
            .collect())
    }

    /// The schema state the last migration left behind, or the empty schema when
    /// there are none — what the next migration is diffed against.
    pub fn latest_snapshot(&self) -> Result<Snapshot> {
        Ok(self
            .load()?
            .last()
            .map(|m| m.snapshot.clone())
            .unwrap_or_else(Snapshot::empty))
    }

    /// Author the next migration from the current `models` and write it to the
    /// directory, returning what was written — or `None` when the models already
    /// match the latest snapshot ("no changes detected"), in which case nothing
    /// is written.
    ///
    /// The backend must match the directory's existing migrations; mixing
    /// dialects in one history is an [`Error::History`].
    pub fn make(
        &self,
        models: &[&'static ModelMeta],
        name: &str,
        backend: Backend,
    ) -> Result<Option<StoredMigration>> {
        let existing = self.load()?;
        let want = backend_str(backend);
        if let Some(first) = existing.first() {
            if first.backend != want {
                return Err(Error::History(format!(
                    "this history holds `{}` migrations; cannot add a `{want}` one",
                    first.backend
                )));
            }
        }

        let previous = existing
            .last()
            .map(|m| m.snapshot.clone())
            .unwrap_or_else(Snapshot::empty);
        let version = existing.last().map(|m| m.version + 1).unwrap_or(1);

        let Some((migration, snapshot)) =
            make_migration(&previous, models, version, name, backend)?
        else {
            return Ok(None);
        };

        let stored = StoredMigration {
            version,
            name: name.to_string(),
            backend: want.to_string(),
            statements: migration.statements,
            snapshot,
        };
        self.write(&stored)?;
        Ok(Some(stored))
    }

    /// Write one migration as `NNNN_<name>.json`, creating the directory if
    /// needed. The version is zero-padded so a lexical sort of the filenames
    /// matches the numeric order.
    fn write(&self, m: &StoredMigration) -> Result<()> {
        std::fs::create_dir_all(&self.dir)
            .map_err(|e| Error::History(format!("cannot create {}: {e}", self.dir.display())))?;
        let file = self
            .dir
            .join(format!("{:04}_{}.json", m.version, slug(&m.name)));
        let json = serde_json::to_string_pretty(m)
            .map_err(|e| Error::History(format!("cannot serialise migration: {e}")))?;
        std::fs::write(&file, json)
            .map_err(|e| Error::History(format!("cannot write {}: {e}", file.display())))?;
        Ok(())
    }
}

/// `"postgres"` or `"sqlite"` for a backend.
fn backend_str(backend: Backend) -> &'static str {
    match backend {
        Backend::Postgres => "postgres",
        Backend::Sqlite => "sqlite",
    }
}

/// Make a filename-safe slug of a migration name: lowercase, and every run of
/// non-alphanumeric characters collapsed to a single `_`.
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_underscore = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_underscore = false;
        } else if !last_underscore {
            out.push('_');
            last_underscore = true;
        }
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "migration".to_string()
    } else {
        trimmed.to_string()
    }
}

/// The first version that appears more than once in a version-sorted slice.
fn first_duplicate(sorted: &[StoredMigration]) -> Option<i64> {
    sorted
        .windows(2)
        .find(|w| w[0].version == w[1].version)
        .map(|w| w[0].version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dorm_core::meta::FieldMeta;
    use dorm_core::value::FieldType;

    // A scratch directory under the system temp dir, removed on drop.
    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("dorm-hist-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const fn col(name: &'static str, ty: FieldType) -> FieldMeta {
        FieldMeta {
            name,
            column: name,
            ty,
            nullable: false,
            primary_key: false,
            unique: false,
            index: false,
            max_length: None,
            default: None,
            auto_now_add: false,
            auto_now: false,
            relation: None,
        }
    }
    const ID: FieldMeta = FieldMeta {
        primary_key: true,
        ..col("id", FieldType::I64)
    };
    const BODY: FieldMeta = FieldMeta {
        nullable: true,
        ..col("body", FieldType::Text)
    };
    const V1: &[FieldMeta] = &[ID, col("title", FieldType::Text)];
    const V2: &[FieldMeta] = &[ID, col("title", FieldType::Text), BODY];

    fn model(fields: &'static [FieldMeta]) -> ModelMeta {
        ModelMeta {
            name: "Note",
            table: "notes",
            fields,
            ordering: &[],
            tenant_scoped: false,
            unique_together: &[],
            checks: &[],
        }
    }

    #[test]
    fn make_then_make_builds_a_chain_each_diffing_the_last() {
        let tmp = TempDir::new();
        let h = History::open(&tmp.0);
        let v1: &'static ModelMeta = Box::leak(Box::new(model(V1)));
        let v2: &'static ModelMeta = Box::leak(Box::new(model(V2)));

        // First make: an initial migration, version 1.
        let m1 = h
            .make(&[v1], "initial", Backend::Postgres)
            .unwrap()
            .expect("there are changes");
        assert_eq!(m1.version, 1);
        assert!(m1.statements[0].starts_with("CREATE TABLE"), "{m1:?}");

        // Second make: diffs v1's snapshot, so it is just the ADD COLUMN.
        let m2 = h
            .make(&[v2], "add body", Backend::Postgres)
            .unwrap()
            .expect("there are changes");
        assert_eq!(m2.version, 2);
        assert_eq!(m2.statements.len(), 1, "{m2:?}");
        assert!(m2.statements[0].contains("ADD COLUMN"), "{m2:?}");

        // The chain on disk is both, in order.
        let chain = h.load().unwrap();
        assert_eq!(
            chain.iter().map(|m| m.version).collect::<Vec<_>>(),
            vec![1, 2]
        );
        // The slug sanitised the space in "add body".
        assert!(tmp.0.join("0002_add_body.json").exists());
    }

    #[test]
    fn a_second_make_with_no_change_writes_nothing() {
        let tmp = TempDir::new();
        let h = History::open(&tmp.0);
        let v1: &'static ModelMeta = Box::leak(Box::new(model(V1)));
        h.make(&[v1], "initial", Backend::Postgres)
            .unwrap()
            .unwrap();
        let none = h.make(&[v1], "again", Backend::Postgres).unwrap();
        assert!(none.is_none(), "an unchanged schema makes no migration");
        assert_eq!(h.load().unwrap().len(), 1, "nothing new was written");
    }

    #[test]
    fn a_missing_directory_is_an_empty_history() {
        let h = History::open(std::env::temp_dir().join("dorm-hist-does-not-exist-xyz"));
        assert!(h.load().unwrap().is_empty());
        assert_eq!(h.latest_snapshot().unwrap(), Snapshot::empty());
    }

    #[test]
    fn mixing_backends_in_one_history_is_refused() {
        let tmp = TempDir::new();
        let h = History::open(&tmp.0);
        let v1: &'static ModelMeta = Box::leak(Box::new(model(V1)));
        h.make(&[v1], "initial", Backend::Postgres)
            .unwrap()
            .unwrap();
        let v2: &'static ModelMeta = Box::leak(Box::new(model(V2)));
        let err = h.make(&[v2], "add", Backend::Sqlite).unwrap_err();
        assert!(matches!(err, Error::History(_)), "got {err:?}");
    }

    #[test]
    fn a_stored_migration_round_trips_through_the_directory() {
        let tmp = TempDir::new();
        let h = History::open(&tmp.0);
        let v1: &'static ModelMeta = Box::leak(Box::new(model(V1)));
        let written = h.make(&[v1], "initial", Backend::Sqlite).unwrap().unwrap();
        let loaded = h.load().unwrap();
        assert_eq!(loaded, vec![written], "what was written is what loads back");
        assert_eq!(loaded[0].backend, "sqlite");
    }

    #[test]
    fn the_slug_is_filename_safe() {
        assert_eq!(slug("Add body"), "add_body");
        assert_eq!(slug("tenants & apps!!"), "tenants_apps");
        assert_eq!(slug("  "), "migration");
        assert_eq!(slug("already_ok"), "already_ok");
    }
}
