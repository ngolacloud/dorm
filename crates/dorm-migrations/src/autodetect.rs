//! The autodetector: the next versioned migration, from a diff of the schema's
//! **previous state** against the **current models**.
//!
//! This is the shape Django's `makemigrations` and Prisma's `migrate dev` have,
//! and the piece [ADR 0072] and `dorm-models` always promised. Where
//! [`crate::sync`]/[`crate::diff`] compare the models against a *live database*
//! and only add (the `prisma db push` shape), this compares them against a
//! stored [`Snapshot`] of what the last migration left behind — so it has
//! history, and it can emit drops as well as adds.
//!
//! The [`Snapshot`] is serialisable (serde): a migration stores the snapshot it
//! produced, and the next `make_migration` diffs against it. That snapshot chain
//! is to DORM what the migration *state* is to Django and the migration *history*
//! is to Prisma.
//!
//! # What this version generates
//!
//! `CREATE TABLE` / `DROP TABLE`, `ADD COLUMN` / `DROP COLUMN`, and
//! `CREATE INDEX` / `DROP INDEX` — all dialect-aware and, where the dialect
//! allows, idempotent (`IF NOT EXISTS` / `IF EXISTS`), so a generated migration
//! can adopt a schema that already carries part of the change.
//!
//! # In-place column alterations
//!
//! A column whose **type or nullability** changed is altered in place:
//! PostgreSQL gets a targeted `ALTER COLUMN … TYPE` / `SET`/`DROP NOT NULL`;
//! SQLite — which cannot `ALTER COLUMN` at all — gets a **table rebuild** (build
//! the new shape under a temporary name, copy the shared columns, drop the old
//! table, rename the new one in, recreate the indexes), the procedure the SQLite
//! docs prescribe. The rebuild also absorbs that table's column adds/drops and
//! index changes, so they are not emitted twice.
//!
//! # What it still refuses rather than get wrong
//!
//! On PostgreSQL, a change to a column's **uniqueness, default, primary key or
//! foreign key**, and any change to a table's composite `UNIQUE`/`CHECK`, are
//! returned as [`Error::UnsupportedChange`] — those need named constraints this
//! version does not yet manage. (SQLite handles them via the rebuild.) Following
//! the roadmap's rule: unsupported capabilities return an explicit error, never
//! a wrong result.
//!
//! [ADR 0072]: ../../docs/adr/0072-arquitectura-alvo-de-crates-e-o-orm-como-camada-de-persistencia.md

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ddl::{self, Backend};
use crate::migrator::Migration;
use crate::{Error, Result};
use dorm_core::meta::{FieldMeta, ModelMeta};
use dorm_core::value::FieldType;

/// A serialisable picture of the schema as the models describe it — the state a
/// migration leaves behind, to diff the next set of models against.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Snapshot {
    pub tables: Vec<Table>,
}

/// One table in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// Composite `UNIQUE` groups, each a list of column names.
    pub unique_together: Vec<Vec<String>>,
    /// Table-level `CHECK` expressions.
    pub checks: Vec<String>,
}

/// One column in a [`Table`]. Carries exactly the attributes the autodetector
/// compares; the `index` flag is compared separately from the rest because an
/// index add/drop is a statement of its own, not a column alteration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: String,
    pub nullable: bool,
    pub unique: bool,
    pub primary_key: bool,
    pub index: bool,
    pub default: Option<String>,
    pub auto_now_add: bool,
    pub auto_now: bool,
    /// The model a foreign key points at, by name, or `None` for a plain column.
    pub references: Option<String>,
}

impl Snapshot {
    /// The empty schema — the state before the first migration. Diffing the
    /// current models against this yields a full `CREATE TABLE` for each, i.e.
    /// the initial migration.
    pub fn empty() -> Self {
        Snapshot::default()
    }

    /// The snapshot the given models describe. Pure: it reads only the metadata,
    /// in model and field order, so the result is deterministic.
    pub fn from_models(models: &[&'static ModelMeta]) -> Self {
        Snapshot {
            tables: models.iter().map(|m| table_of(m)).collect(),
        }
    }
}

fn table_of(m: &ModelMeta) -> Table {
    Table {
        name: m.table.to_string(),
        columns: m.fields.iter().map(column_of).collect(),
        unique_together: m
            .unique_together
            .iter()
            .map(|g| g.iter().map(ToString::to_string).collect())
            .collect(),
        checks: m.checks.iter().map(ToString::to_string).collect(),
    }
}

fn column_of(f: &FieldMeta) -> Column {
    Column {
        name: f.column.to_string(),
        ty: type_tag(f.ty).to_string(),
        nullable: f.nullable,
        unique: f.unique,
        primary_key: f.primary_key,
        index: f.index,
        default: f.default.map(ToString::to_string),
        auto_now_add: f.auto_now_add,
        auto_now: f.auto_now,
        references: f.relation.map(|r| r.model.to_string()),
    }
}

/// A stable string tag for a field type, so a snapshot survives serialisation
/// without depending on the core enum's representation.
fn type_tag(ty: FieldType) -> &'static str {
    match ty {
        FieldType::I32 => "i32",
        FieldType::I64 => "i64",
        FieldType::Text => "text",
        FieldType::Bool => "bool",
        FieldType::Decimal => "decimal",
        FieldType::Timestamp => "timestamp",
        FieldType::Uuid => "uuid",
        FieldType::Json => "json",
    }
}

/// Whether two columns are the same in every respect **except** whether they are
/// indexed — an index change is emitted as its own `CREATE`/`DROP INDEX`, not as
/// a column alteration.
fn same_except_index(a: &Column, b: &Column) -> bool {
    Column {
        index: false,
        ..a.clone()
    } == Column {
        index: false,
        ..b.clone()
    }
}

/// The ordered DDL to take the schema from `previous` to what `models` describe,
/// for `backend`. Pure and database-free — the same discipline as the query
/// compiler's snapshot tests.
///
/// The statements are ordered additive-before-destructive so a run never drops
/// something a later statement still needs: create tables, add columns, create
/// indexes, alter columns in place, rebuild tables, then drop indexes, columns
/// and tables. An in-place alteration PostgreSQL cannot express as a plain
/// `ALTER COLUMN TYPE`/`SET NOT NULL` is still an [`Error::UnsupportedChange`].
pub fn autodetect(
    previous: &Snapshot,
    models: &[&'static ModelMeta],
    backend: Backend,
) -> Result<Vec<String>> {
    let current = Snapshot::from_models(models);
    let prev_tables: BTreeMap<&str, &Table> = previous
        .tables
        .iter()
        .map(|t| (t.name.as_str(), t))
        .collect();
    let model_by_table: BTreeMap<&str, &&'static ModelMeta> =
        models.iter().map(|m| (m.table, m)).collect();

    let mut b = Buckets::default();

    // Tables the models declare: new ones get a full CREATE; existing ones get a
    // column-, index- and alteration-level diff.
    for (cur_table, meta) in current.tables.iter().zip(models.iter()) {
        match prev_tables.get(cur_table.name.as_str()) {
            None => {
                b.creates.push(ddl::create_table_on(meta, backend)?);
                b.creates.extend(ddl::indexes(meta));
            }
            Some(prev) => diff_table(prev, cur_table, meta, backend, &mut b)?,
        }
    }

    // Tables the previous snapshot had but no model declares any more.
    for prev in &previous.tables {
        if !model_by_table.contains_key(prev.name.as_str()) {
            b.table_drops.push(ddl::drop_table(&prev.name));
        }
    }

    Ok(b.into_ordered())
}

/// The statement buckets, kept apart so the whole migration can be emitted
/// additive-before-destructive regardless of the order tables are visited in.
#[derive(Default)]
struct Buckets {
    creates: Vec<String>,
    adds: Vec<String>,
    index_adds: Vec<String>,
    pg_alters: Vec<String>,
    rebuilds: Vec<String>,
    index_drops: Vec<String>,
    col_drops: Vec<String>,
    table_drops: Vec<String>,
}

impl Buckets {
    fn into_ordered(self) -> Vec<String> {
        let mut out = self.creates;
        out.extend(self.adds);
        out.extend(self.index_adds);
        out.extend(self.pg_alters);
        out.extend(self.rebuilds);
        out.extend(self.index_drops);
        out.extend(self.col_drops);
        out.extend(self.table_drops);
        out
    }
}

/// Diff one table that exists in both the previous snapshot and the current
/// models, appending the statements to the right ordered bucket.
fn diff_table(
    prev: &Table,
    cur: &Table,
    meta: &'static ModelMeta,
    backend: Backend,
    b: &mut Buckets,
) -> Result<()> {
    let constraint_changed =
        prev.unique_together != cur.unique_together || prev.checks != cur.checks;

    let prev_cols: BTreeMap<&str, &Column> =
        prev.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let cur_cols: BTreeMap<&str, &Column> =
        cur.columns.iter().map(|c| (c.name.as_str(), c)).collect();
    let field_by_col: BTreeMap<&str, &FieldMeta> =
        meta.fields.iter().map(|f| (f.column, f)).collect();

    // Columns present in both whose definition (ignoring the index flag) changed.
    let altered: Vec<(&Column, &Column)> = cur
        .columns
        .iter()
        .filter_map(|c| prev_cols.get(c.name.as_str()).map(|p| (*p, c)))
        .filter(|(p, c)| !same_except_index(p, c))
        .collect();

    // SQLite cannot `ALTER COLUMN` or change a table constraint in place; when
    // either is needed, the whole table is rebuilt — which subsumes the column
    // adds, drops and index changes too, so nothing else is emitted for it.
    if backend == Backend::Sqlite && (!altered.is_empty() || constraint_changed) {
        b.rebuilds.extend(sqlite_rebuild(prev, cur, meta)?);
        return Ok(());
    }

    // PostgreSQL (or SQLite with no structural change): targeted statements.
    if constraint_changed {
        // Reachable only on PostgreSQL here — composite UNIQUE/CHECK needs
        // ADD/DROP CONSTRAINT, which this version does not yet name and manage.
        return Err(Error::UnsupportedChange(format!(
            "table `{}`: its composite UNIQUE/CHECK constraints changed",
            cur.name
        )));
    }
    for (prev_col, cur_col) in &altered {
        let field = field_by_col
            .get(cur_col.name.as_str())
            .expect("an altered column came from this model's fields");
        b.pg_alters
            .extend(pg_column_alters(prev_col, cur_col, &cur.name, field)?);
    }

    for cur_col in &cur.columns {
        match prev_cols.get(cur_col.name.as_str()) {
            None => {
                // A new column. Render it from the live field (so a foreign key
                // resolves its target through the registry), and add its index
                // too if it asked for one.
                let f = field_by_col
                    .get(cur_col.name.as_str())
                    .expect("the snapshot column came from this model's fields");
                b.adds.push(ddl::add_column_on(meta, f, backend)?);
                if cur_col.index {
                    b.index_adds.push(ddl::index_on(&cur.name, &cur_col.name));
                }
            }
            Some(prev_col) => match (prev_col.index, cur_col.index) {
                (false, true) => b.index_adds.push(ddl::index_on(&cur.name, &cur_col.name)),
                (true, false) => b
                    .index_drops
                    .push(ddl::drop_index(&cur.name, &cur_col.name)),
                _ => {}
            },
        }
    }

    // Columns the previous snapshot had and the model dropped. Dropping the
    // column drops its index with it, so no separate DROP INDEX is needed.
    for prev_col in &prev.columns {
        if !cur_cols.contains_key(prev_col.name.as_str()) {
            b.col_drops
                .push(ddl::drop_column(&cur.name, &prev_col.name, backend));
        }
    }

    Ok(())
}

/// The SQLite table rebuild — the only way SQLite can alter a column or change a
/// table constraint. It follows the procedure the SQLite docs prescribe, reduced
/// to what a model-defined schema needs: build the new shape under a temporary
/// name, copy the columns the two versions share, drop the old table, rename the
/// new one into its place, and recreate its indexes.
///
/// Foreign-key *enforcement* is left off throughout (the ORM never turns
/// `PRAGMA foreign_keys` on), so the drop-and-rename does not trip an inbound
/// reference mid-rebuild; the whole sequence runs inside the migrator's
/// transaction, so it is all-or-nothing.
///
/// Columns present only in the new shape are absent from the copy and take their
/// default or `NULL`; a new `NOT NULL` column with no default is therefore only
/// safe on an empty table, the same caveat as `ADD COLUMN`.
fn sqlite_rebuild(prev: &Table, cur: &Table, meta: &'static ModelMeta) -> Result<Vec<String>> {
    let tmp = format!("{}__dorm_new", cur.name);
    let prev_names: std::collections::HashSet<&str> =
        prev.columns.iter().map(|c| c.name.as_str()).collect();
    // Shared columns in the NEW table's order, so the INSERT and SELECT lists
    // line up and a changed type is copied across SQLite's loose typing.
    let shared: Vec<&str> = cur
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .filter(|n| prev_names.contains(n))
        .collect();

    let mut out = vec![ddl::create_table_named_on(meta, &tmp, Backend::Sqlite)?];
    if !shared.is_empty() {
        let cols = shared.join(", ");
        out.push(format!(
            "INSERT INTO {tmp} ({cols}) SELECT {cols} FROM {}",
            cur.name
        ));
    }
    out.push(format!("DROP TABLE {}", cur.name));
    out.push(format!("ALTER TABLE {tmp} RENAME TO {}", cur.name));
    out.extend(ddl::indexes(meta));
    Ok(out)
}

/// The PostgreSQL `ALTER COLUMN` statements for one changed column, when the only
/// things that changed are its type and/or its nullability — the two alterations
/// a plain `ALTER COLUMN` expresses safely. Any other change (uniqueness,
/// default, primary key, or a foreign key) is an [`Error::UnsupportedChange`]:
/// those need named constraints this version does not yet manage.
fn pg_column_alters(
    prev: &Column,
    cur: &Column,
    table: &str,
    field: &FieldMeta,
) -> Result<Vec<String>> {
    // Everything but type and nullability (and the index flag, handled apart)
    // must be identical; otherwise this is not an alteration we generate.
    let normalised = Column {
        ty: cur.ty.clone(),
        nullable: cur.nullable,
        index: cur.index,
        ..prev.clone()
    };
    if normalised != *cur {
        return Err(Error::UnsupportedChange(format!(
            "column `{}` of table `{table}`: a change to uniqueness, default, \
             primary key or foreign key — only type and nullability are generated",
            cur.name
        )));
    }

    let mut out = Vec::new();
    if prev.ty != cur.ty {
        if field.relation.is_some() {
            return Err(Error::UnsupportedChange(format!(
                "column `{}` of table `{table}`: changing a foreign key's type",
                cur.name
            )));
        }
        out.push(format!(
            "ALTER TABLE {table} ALTER COLUMN {} TYPE {}",
            cur.name,
            field.ty.sql_type()
        ));
    }
    if prev.nullable != cur.nullable {
        let verb = if cur.nullable {
            "DROP NOT NULL"
        } else {
            "SET NOT NULL"
        };
        out.push(format!(
            "ALTER TABLE {table} ALTER COLUMN {} {verb}",
            cur.name
        ));
    }
    Ok(out)
}

/// The next versioned migration and the snapshot it leaves behind, or `None`
/// when the models already match `previous` ("no changes detected").
///
/// Store the returned [`Snapshot`] alongside the migration; it is what the
/// *next* `make_migration` diffs against. The `version`/`name` are the caller's
/// to choose, exactly as Django names a migration and Prisma names a folder.
pub fn make_migration(
    previous: &Snapshot,
    models: &[&'static ModelMeta],
    version: i64,
    name: &str,
    backend: Backend,
) -> Result<Option<(Migration, Snapshot)>> {
    let statements = autodetect(previous, models, backend)?;
    if statements.is_empty() {
        return Ok(None);
    }
    let migration = Migration {
        version,
        name: name.to_string(),
        statements,
    };
    Ok(Some((migration, Snapshot::from_models(models))))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Hand-built metadata, no registry and no derive — the same approach as the
    // ddl tests. Relation-free on purpose: a foreign key would need the target
    // registered, which these pure tests avoid. FK coverage is in the e2e/pilot.
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

    // A model with id + title.
    const TITLE: FieldMeta = col("title", FieldType::Text);
    const NOTE_V1: &[FieldMeta] = &[ID, TITLE];
    const NOTE_V1_META: ModelMeta = ModelMeta {
        name: "Note",
        table: "notes",
        fields: NOTE_V1,
        ordering: &[],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    };

    // The same model, with a new `body` column added and `title` now indexed.
    const BODY: FieldMeta = col("body", FieldType::Text);
    const TITLE_INDEXED: FieldMeta = FieldMeta {
        index: true,
        ..col("title", FieldType::Text)
    };
    const NOTE_V2: &[FieldMeta] = &[ID, TITLE_INDEXED, BODY];
    const NOTE_V2_META: ModelMeta = ModelMeta {
        name: "Note",
        table: "notes",
        fields: NOTE_V2,
        ordering: &[],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    };

    // A v2 whose new column is nullable — the shape that is safe to add to a
    // table that already holds rows (a NOT NULL column with no default is not,
    // which is a known v1 limitation the execution test deliberately avoids).
    const BODY_NULLABLE: FieldMeta = FieldMeta {
        nullable: true,
        ..col("body", FieldType::Text)
    };
    const NOTE_V2_NULLABLE: &[FieldMeta] = &[ID, TITLE, BODY_NULLABLE];
    const NOTE_V2_NULLABLE_META: ModelMeta = ModelMeta {
        name: "Note",
        table: "notes",
        fields: NOTE_V2_NULLABLE,
        ordering: &[],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    };

    #[test]
    fn against_the_empty_snapshot_it_is_a_full_create() {
        let stmts = autodetect(&Snapshot::empty(), &[&NOTE_V1_META], Backend::Postgres).unwrap();
        assert_eq!(stmts.len(), 1, "{stmts:?}");
        assert!(
            stmts[0].starts_with("CREATE TABLE IF NOT EXISTS notes"),
            "{stmts:?}"
        );
    }

    #[test]
    fn an_added_column_and_a_new_index_are_detected_in_order() {
        let prev = Snapshot::from_models(&[&NOTE_V1_META]);
        let stmts = autodetect(&prev, &[&NOTE_V2_META], Backend::Postgres).unwrap();
        // ADD COLUMN body, then CREATE INDEX on the newly-indexed title.
        assert_eq!(stmts.len(), 2, "{stmts:?}");
        assert_eq!(
            stmts[0],
            "ALTER TABLE notes ADD COLUMN IF NOT EXISTS body TEXT NOT NULL"
        );
        assert_eq!(
            stmts[1],
            "CREATE INDEX IF NOT EXISTS notes_title_idx ON notes (title)"
        );
    }

    #[test]
    fn a_dropped_column_and_a_dropped_table_come_last() {
        // v2 → v1: lose the `body` column and the `title` index; and drop a
        // whole second table that v1 had.
        const OTHER: ModelMeta = ModelMeta {
            name: "Tag",
            table: "tags",
            fields: NOTE_V1,
            ordering: &[],
            tenant_scoped: false,
            unique_together: &[],
            checks: &[],
        };
        let prev = Snapshot::from_models(&[&NOTE_V2_META, &OTHER]);
        let stmts = autodetect(&prev, &[&NOTE_V1_META], Backend::Postgres).unwrap();
        // DROP INDEX title (lost its index), DROP COLUMN body, DROP TABLE tags.
        assert_eq!(
            stmts,
            vec![
                "DROP INDEX IF EXISTS notes_title_idx".to_string(),
                "ALTER TABLE notes DROP COLUMN IF EXISTS body".to_string(),
                "DROP TABLE IF EXISTS tags".to_string(),
            ],
            "{stmts:?}"
        );
    }

    #[test]
    fn sqlite_omits_the_if_clauses_the_dialect_rejects() {
        let prev = Snapshot::from_models(&[&NOTE_V1_META]);
        let stmts = autodetect(&prev, &[&NOTE_V2_META], Backend::Sqlite).unwrap();
        assert_eq!(
            stmts[0], "ALTER TABLE notes ADD COLUMN body TEXT NOT NULL",
            "SQLite ADD COLUMN takes no IF NOT EXISTS: {stmts:?}"
        );
    }

    // title: Text → I64, plus made nullable — a type and a nullability change.
    const TITLE_AS_INT: FieldMeta = FieldMeta {
        nullable: true,
        ..col("title", FieldType::I64)
    };
    const RETYPED_FIELDS: &[FieldMeta] = &[ID, TITLE_AS_INT];
    const RETYPED_META: ModelMeta = ModelMeta {
        name: "Note",
        table: "notes",
        fields: RETYPED_FIELDS,
        ordering: &[],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    };

    #[test]
    fn postgres_alters_type_and_nullability_in_place() {
        let prev = Snapshot::from_models(&[&NOTE_V1_META]);
        let stmts = autodetect(&prev, &[&RETYPED_META], Backend::Postgres).unwrap();
        assert_eq!(
            stmts,
            vec![
                "ALTER TABLE notes ALTER COLUMN title TYPE BIGINT".to_string(),
                "ALTER TABLE notes ALTER COLUMN title DROP NOT NULL".to_string(),
            ],
            "{stmts:?}"
        );
    }

    #[test]
    fn sqlite_rebuilds_the_table_to_alter_a_column() {
        let prev = Snapshot::from_models(&[&NOTE_V1_META]);
        let stmts = autodetect(&prev, &[&RETYPED_META], Backend::Sqlite).unwrap();
        // Create the new shape, copy the shared columns, drop, rename.
        assert!(
            stmts[0].starts_with("CREATE TABLE IF NOT EXISTS notes__dorm_new"),
            "{stmts:?}"
        );
        assert_eq!(
            stmts[1],
            "INSERT INTO notes__dorm_new (id, title) SELECT id, title FROM notes"
        );
        assert_eq!(stmts[2], "DROP TABLE notes");
        assert_eq!(stmts[3], "ALTER TABLE notes__dorm_new RENAME TO notes");
    }

    #[test]
    fn postgres_refuses_a_change_it_cannot_name_a_constraint_for() {
        // title gains UNIQUE — an ADD CONSTRAINT this version does not manage.
        const TITLE_UNIQUE: FieldMeta = FieldMeta {
            unique: true,
            ..col("title", FieldType::Text)
        };
        const UNIQ_FIELDS: &[FieldMeta] = &[ID, TITLE_UNIQUE];
        const UNIQ_META: ModelMeta = ModelMeta {
            name: "Note",
            table: "notes",
            fields: UNIQ_FIELDS,
            ordering: &[],
            tenant_scoped: false,
            unique_together: &[],
            checks: &[],
        };
        let prev = Snapshot::from_models(&[&NOTE_V1_META]);
        let err = autodetect(&prev, &[&UNIQ_META], Backend::Postgres).unwrap_err();
        assert!(
            matches!(err, Error::UnsupportedChange(_)),
            "a uniqueness change must still be an explicit error on PG, got {err:?}"
        );
    }

    // title stays TEXT but loses its NOT NULL — an alteration SQLite can only do
    // by rebuilding the table.
    const TITLE_NULLABLE: FieldMeta = FieldMeta {
        nullable: true,
        ..col("title", FieldType::Text)
    };
    const RELAXED_FIELDS: &[FieldMeta] = &[ID, TITLE_NULLABLE];
    const RELAXED_META: ModelMeta = ModelMeta {
        name: "Note",
        table: "notes",
        fields: RELAXED_FIELDS,
        ordering: &[],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    };

    #[test]
    fn a_sqlite_rebuild_runs_and_keeps_the_rows() {
        // The capstone for the rebuild: not that the SQL reads right, but that it
        // executes, preserves the data, and really dropped the NOT NULL.
        let conn = rusqlite::Connection::open_in_memory().expect("open sqlite");
        for s in autodetect(&Snapshot::empty(), &[&NOTE_V1_META], Backend::Sqlite).unwrap() {
            conn.execute_batch(&s).expect("apply v1");
        }
        conn.execute("INSERT INTO notes (title) VALUES ('hello')", [])
            .expect("insert a v1 row");

        // title NOT NULL → nullable forces a table rebuild.
        let prev = Snapshot::from_models(&[&NOTE_V1_META]);
        for s in autodetect(&prev, &[&RELAXED_META], Backend::Sqlite).unwrap() {
            conn.execute_batch(&s).expect("apply the rebuild");
        }

        // The old row survived with its title intact...
        let title: String = conn
            .query_row("SELECT title FROM notes", [], |r| r.get(0))
            .expect("the row survived the rebuild");
        assert_eq!(title, "hello");
        // ...and a NULL title is now accepted, which it was not before.
        conn.execute("INSERT INTO notes (title) VALUES (NULL)", [])
            .expect("the NOT NULL was dropped");
        let total: i64 = conn
            .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2);
    }

    #[test]
    fn no_changes_make_no_migration() {
        let prev = Snapshot::from_models(&[&NOTE_V1_META]);
        let made = make_migration(&prev, &[&NOTE_V1_META], 2, "noop", Backend::Postgres).unwrap();
        assert!(made.is_none(), "an unchanged schema yields no migration");
    }

    #[test]
    fn make_migration_versions_and_returns_the_next_snapshot() {
        let prev = Snapshot::from_models(&[&NOTE_V1_META]);
        let (migration, snapshot) =
            make_migration(&prev, &[&NOTE_V2_META], 2, "add_body", Backend::Postgres)
                .unwrap()
                .expect("there are changes");
        assert_eq!(migration.version, 2);
        assert_eq!(migration.name, "add_body");
        assert_eq!(migration.statements.len(), 2);
        // The returned snapshot is the new state — diffing it against the same
        // models now yields nothing.
        assert!(autodetect(&snapshot, &[&NOTE_V2_META], Backend::Postgres)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn the_generated_sqlite_migration_runs_and_preserves_the_data() {
        // The capstone: not that the SQL looks right, but that it IS valid SQL
        // SQLite accepts, and that migrating a populated table keeps its rows.
        let conn = rusqlite::Connection::open_in_memory().expect("open sqlite");

        // v1: create the table from the empty snapshot, and put a row in it.
        for s in autodetect(&Snapshot::empty(), &[&NOTE_V1_META], Backend::Sqlite).unwrap() {
            conn.execute_batch(&s).expect("apply v1");
        }
        conn.execute("INSERT INTO notes (title) VALUES ('first')", [])
            .expect("insert a v1 row");

        // v2: add the nullable `body`. The generated ALTER must run against the
        // already-populated table.
        let prev = Snapshot::from_models(&[&NOTE_V1_META]);
        let v2 = autodetect(&prev, &[&NOTE_V2_NULLABLE_META], Backend::Sqlite).unwrap();
        assert_eq!(
            v2,
            vec!["ALTER TABLE notes ADD COLUMN body TEXT".to_string()]
        );
        for s in &v2 {
            conn.execute_batch(s)
                .expect("apply v2 to a populated table");
        }

        // The old row survived (body NULL), and the new column takes a value.
        conn.execute(
            "INSERT INTO notes (title, body) VALUES ('second', 'has body')",
            [],
        )
        .expect("insert using the new column");
        let total: i64 = conn
            .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 2, "the v1 row survived the migration");
        let with_body: i64 = conn
            .query_row(
                "SELECT count(*) FROM notes WHERE body IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(with_body, 1, "only the new row has a body");
    }

    #[test]
    fn a_snapshot_round_trips_through_json() {
        let snap = Snapshot::from_models(&[&NOTE_V2_META]);
        let json = serde_json::to_string(&snap).expect("serialise");
        let back: Snapshot = serde_json::from_str(&json).expect("deserialise");
        assert_eq!(snap, back, "the snapshot survives a JSON round trip");
    }
}
