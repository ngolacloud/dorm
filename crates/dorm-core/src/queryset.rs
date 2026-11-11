//! `Manager` and `QuerySet` — the typed front end.
//!
//! A `QuerySet` is lazy and immutable: every method consumes `self` and returns a
//! new one, and nothing reaches the database until a terminal method is awaited.
//! That is what makes `let base = Tenant::objects().filter(…);` reusable — two
//! branches built from `base` cannot disturb each other, because neither mutated
//! it.

use std::collections::BTreeMap;
use std::marker::PhantomData;

use crate::compiler::{self, Plan, Sql};
use crate::error::{Error, Result};
use crate::expr::{AggKind, Aggregate, Assignment, OrderBy, Q};
use crate::model::{insert_returning, run_execute, run_query, Model, NewRow};
use crate::row::Row;
use crate::value::{read_column, ToValue, Value};
use crate::{cursor, db, registry, scope};

/// One row of a `values()` query: column name to value, in a stable order.
pub type ValueRow = BTreeMap<String, Value>;

/// One page of a keyset-paginated query (ADR 0024): the rows, and an opaque
/// signed cursor for the next page — `None` when this page was not full, i.e. the
/// end was reached.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

/// A stable fingerprint of the query a cursor belongs to (model, ordering,
/// filters) — **not** including the keyset state or limit, so the cursor minted
/// by one page validates on the next. A cursor whose fingerprint does not match is
/// refused by [`cursor::decode`].
fn keyset_fingerprint(plan: &Plan) -> String {
    let order: Vec<String> = if plan.order.is_empty() {
        plan.model
            .ordering
            .iter()
            .map(|s| (*s).to_string())
            .collect()
    } else {
        plan.order
            .iter()
            .map(|t| format!("{}{}", if t.descending { "-" } else { "" }, t.path))
            .collect()
    };
    format!(
        "m={};o={};f={:?};x={:?}",
        plan.model.name,
        order.join(","),
        plan.filters,
        plan.excludes
    )
}

/// The ordering-key values of `row`, in effective-order order (the order terms,
/// then the primary key) — what the next page's cursor encodes. Keyset keys must
/// be direct columns of the model, so each is read at its own position in the
/// select list (which is the model's field order).
fn keyset_boundary(plan: &Plan, row: &Row) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for term in plan.effective_order() {
        let (idx, field) = plan
            .model
            .fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.name == term.path)
            .ok_or_else(|| {
                Error::Config(format!(
                    "keyset pagination orders by `{}`, which is not a direct column of `{}`",
                    term.path, plan.model.name
                ))
            })?;
        out.push(read_column(row, idx, field.ty, field.column)?);
    }
    Ok(out)
}

/// The entry point for a model — `Tenant::objects()`.
#[derive(Debug)]
pub struct Manager<T> {
    marker: PhantomData<fn() -> T>,
}

impl<T> Default for Manager<T> {
    fn default() -> Self {
        Manager {
            marker: PhantomData,
        }
    }
}

impl<T: Model> Manager<T> {
    pub fn new() -> Manager<T> {
        Manager::default()
    }

    /// Every row.
    pub fn all(&self) -> QuerySet<T> {
        QuerySet::new()
    }

    /// Narrow by a typed condition.
    pub fn filter(&self, q: Q) -> QuerySet<T> {
        QuerySet::new().filter(q)
    }

    /// Narrow by a string key, for parity with the REPL.
    pub fn filter_str(&self, key: &str, value: impl ToValue) -> QuerySet<T> {
        QuerySet::new().filter_str(key, value)
    }

    /// Widen-then-remove: the complement of `filter`.
    pub fn exclude(&self, q: Q) -> QuerySet<T> {
        QuerySet::new().exclude(q)
    }

    pub fn order_by<I: IntoIterator<Item = OrderBy>>(&self, terms: I) -> QuerySet<T> {
        QuerySet::new().order_by(terms)
    }

    /// Order by string keys, `-field` for descending.
    pub fn order_by_str(&self, terms: &[&str]) -> QuerySet<T> {
        QuerySet::new().order_by_str(terms)
    }

    pub fn annotate(&self, a: Aggregate) -> QuerySet<T> {
        QuerySet::new().annotate(a)
    }

    /// Fetch a foreign key's row in the same query.
    pub fn select_related(&self, field: &str) -> QuerySet<T> {
        QuerySet::new().select_related(field)
    }

    /// Project onto named columns.
    pub fn values(&self, columns: &[&str]) -> QuerySet<T> {
        QuerySet::new().values(columns)
    }

    pub fn distinct(&self) -> QuerySet<T> {
        QuerySet::new().distinct()
    }

    pub fn limit(&self, n: i64) -> QuerySet<T> {
        QuerySet::new().limit(n)
    }

    pub fn offset(&self, n: i64) -> QuerySet<T> {
        QuerySet::new().offset(n)
    }

    /// The unfiltered plan, for callers that need to compile something other
    /// than a `SELECT` — a count, an update, a delete.
    pub fn plan(&self) -> Plan {
        Plan::new(T::meta())
    }

    /// Exactly one row, or an error naming which of the two ways it failed.
    pub async fn get(&self, q: Q) -> Result<T> {
        QuerySet::new().filter(q).get().await
    }

    /// Run hand-written SQL and map each row back to `T` — Django's
    /// `Model.objects.raw` (ADR 0005). The escape hatch for a query the compiler
    /// cannot build (a CTE, a window function, a planner hint).
    ///
    /// The `SELECT` must return the model's columns in the model's field order:
    /// `from_row` reads by position, not by name. Every `params` element is bound
    /// as `$n`/`?n` — no value is formatted into the text, so the only injection
    /// risk is one the caller writes into the SQL string itself.
    ///
    /// ```ignore
    /// let active: Vec<Tenant> = Tenant::objects()
    ///     .raw(
    ///         "SELECT id, slug, name FROM tenants WHERE slug = $1",
    ///         &["acme".to_value()],
    ///     )
    ///     .await?;
    /// ```
    pub async fn raw(&self, sql: &str, params: &[Value]) -> Result<Vec<T>> {
        let rows = db::sql(sql, params).await?;
        rows.iter().map(|row| T::from_row(row, 0)).collect()
    }

    /// Insert one row and return it as stored, with database-generated columns
    /// filled in.
    pub async fn create<N: NewRow<T>>(&self, new: N) -> Result<T> {
        db::guard_write("create")?;
        let values = new.column_values();
        let batch = std::slice::from_ref(&values);
        let sql = scope::compile_insert(T::meta(), batch)?;
        let rows = insert_returning(T::meta(), &sql, batch).await?;
        let row = rows.first().ok_or_else(|| {
            Error::Db("the insert returned no row, so the new key is unknown".into())
        })?;
        T::from_row(row, 0)
    }

    /// Insert many rows, chunked to stay under the wire protocol's parameter
    /// limit.
    ///
    /// One `INSERT … VALUES (…), (…)` per chunk, not a per-row loop: the round
    /// trips are what make a loop slow. But a single statement carries at most
    /// **65 535 bound parameters** (the protocol's `u16` parameter count), and a
    /// row contributes one parameter per column, so one statement tops out at
    /// `floor(65535 / columns)` rows — past that `tokio-postgres` overflows and
    /// the insert fails outright. We chunk at that ceiling. When the rows fit in
    /// a single chunk (the common case) the statement is its own transaction, as
    /// before; when they span several chunks we wrap them in one `atomic` so the
    /// call stays all-or-nothing.
    pub async fn bulk_create<N: NewRow<T>>(&self, rows: Vec<N>) -> Result<Vec<T>> {
        db::guard_write("bulk_create")?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let values: Vec<Vec<(String, Value)>> = rows.iter().map(NewRow::column_values).collect();
        let cols = values[0].len().max(1);
        let per_chunk = (65_535 / cols).max(1);

        // Single chunk: one statement. On a RETURNING backend that is its own
        // implicit transaction, as before; on MySQL `insert_returning` wraps the
        // insert-then-reload in one `atomic` so the two share a connection.
        if values.len() <= per_chunk {
            let sql = scope::compile_insert(T::meta(), &values)?;
            let returned = insert_returning(T::meta(), &sql, &values).await?;
            return returned.iter().map(|r| T::from_row(r, 0)).collect();
        }

        // Several chunks: one statement each, all in one transaction. On MySQL
        // each chunk's `insert_returning` nests as a savepoint within it.
        db::atomic(|_tx| async move {
            let mut out = Vec::with_capacity(values.len());
            for chunk in values.chunks(per_chunk) {
                let sql = scope::compile_insert(T::meta(), chunk)?;
                let returned = insert_returning(T::meta(), &sql, chunk).await?;
                for r in &returned {
                    out.push(T::from_row(r, 0)?);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Fetch the row matching `lookup`, inserting `new` if there is none.
    ///
    /// Returns whether it was created. The insert can still lose a race with a
    /// concurrent caller, so a unique violation is retried as a read rather than
    /// surfaced — which is the only outcome that makes this call idempotent.
    pub async fn get_or_create<N: NewRow<T>>(&self, lookup: Q, new: N) -> Result<(T, bool)> {
        if let Some(found) = QuerySet::<T>::new().filter(lookup.clone()).first().await? {
            return Ok((found, false));
        }
        match self.create_savepointed(new).await {
            Ok(created) => Ok((created, true)),
            Err(e) if crate::error::is_unique_violation(&e) => {
                // Lost the race: the row the other caller inserted is the right
                // answer, and reporting `created = false` is accurate. The insert
                // ran inside its own savepoint (`create_savepointed`), so the
                // unique violation rolled back only that attempt — this recovery
                // read runs on a transaction that is still usable.
                let found = QuerySet::<T>::new()
                    .filter(lookup)
                    .first()
                    .await?
                    .ok_or(e)?;
                Ok((found, false))
            }
            Err(e) => Err(e),
        }
    }

    /// Insert `new` so that a unique violation rolls back only its own attempt,
    /// never a transaction the caller may already be inside.
    ///
    /// The bug this closes: `get_or_create`/`update_or_create` run
    /// `SELECT → INSERT → SELECT`. When the caller wraps the whole thing in
    /// [`db::atomic`], all three share one dedicated
    /// connection. A bare `INSERT` that loses the race fails with `23505` and
    /// **aborts that transaction**, so the recovery `SELECT` comes back with
    /// `25P02 current transaction is aborted` instead of the winner's row — the
    /// promised idempotence is false inside a transaction.
    ///
    /// Wrapping the insert in a nested `atomic` makes it a `SAVEPOINT` when there
    /// is an outer transaction: the `23505` only does `ROLLBACK TO SAVEPOINT`,
    /// leaving the outer transaction usable for the recovery read. With no outer
    /// transaction it is a trivial `BEGIN`/`COMMIT` around the single insert.
    async fn create_savepointed<N: NewRow<T>>(&self, new: N) -> Result<T> {
        db::atomic(|_tx| async move { self.create(new).await }).await
    }

    /// Fetch the row matching `lookup` and apply `updates` to it, or insert `new`
    /// if there is none. Returns whether it was created — Django's
    /// `update_or_create`.
    ///
    /// Like [`get_or_create`](Self::get_or_create), a create that loses a race
    /// with a concurrent caller is recovered by falling back to the update path,
    /// so the call is idempotent. `updates` must not touch the columns `lookup`
    /// filters on — the row is re-read by the same `lookup` to return it.
    pub async fn update_or_create<N: NewRow<T>>(
        &self,
        lookup: Q,
        new: N,
        updates: Vec<Assignment>,
    ) -> Result<(T, bool)> {
        db::guard_write("update_or_create")?;
        if QuerySet::<T>::new()
            .filter(lookup.clone())
            .first()
            .await?
            .is_some()
        {
            return Ok((self.apply_and_fetch(lookup, updates).await?, false));
        }
        match self.create_savepointed(new).await {
            Ok(created) => Ok((created, true)),
            Err(e) if crate::error::is_unique_violation(&e) => {
                // Lost the race: update the row the winner inserted and return it.
                // The insert ran in its own savepoint, so the outer transaction
                // (if any) survives the unique violation and this update can run.
                Ok((self.apply_and_fetch(lookup, updates).await?, false))
            }
            Err(e) => Err(e),
        }
    }

    /// Apply `updates` to the rows matching `lookup` (a no-op when empty) and
    /// return the row `lookup` now selects. Shared by the two found-paths of
    /// [`update_or_create`](Self::update_or_create).
    async fn apply_and_fetch(&self, lookup: Q, updates: Vec<Assignment>) -> Result<T> {
        if !updates.is_empty() {
            QuerySet::<T>::new()
                .filter(lookup.clone())
                .update(updates)
                .await?;
        }
        QuerySet::<T>::new()
            .filter(lookup)
            .first()
            .await?
            .ok_or_else(|| Error::Db("the row vanished during update_or_create".into()))
    }
}

/// A lazy, immutable, chainable query.
pub struct QuerySet<T> {
    plan: Plan,
    marker: PhantomData<fn() -> T>,
}

// `Clone` and `Debug` are written out rather than derived. A derive would put a
// `T: Clone` bound on the impl, and a `QuerySet` holds no `T` at all — only a
// `PhantomData<fn() -> T>` — so the bound would be a lie that makes
// `Tenant::objects().filter(..)` uncloneable unless `Tenant` happens to be
// `Clone`.
impl<T> Clone for QuerySet<T> {
    fn clone(&self) -> Self {
        QuerySet {
            plan: self.plan.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for QuerySet<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The plan is the interesting part, and printing the compiled SQL here
        // would mean a fallible operation inside `Debug`.
        f.debug_struct("QuerySet")
            .field("plan", &self.plan)
            .finish()
    }
}

impl<T: Model> Default for QuerySet<T> {
    fn default() -> Self {
        QuerySet {
            plan: Plan::new(T::meta()),
            marker: PhantomData,
        }
    }
}

impl<T: Model> QuerySet<T> {
    pub fn new() -> QuerySet<T> {
        QuerySet::default()
    }

    /// Build from a plan — the seam the REPL comes in through.
    pub fn from_plan(plan: Plan) -> QuerySet<T> {
        QuerySet {
            plan,
            marker: PhantomData,
        }
    }

    /// The plan, for inspection and for the dynamic front end.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    /// Add a condition. Repeated calls are `AND`ed, as Django's are.
    pub fn filter(mut self, q: Q) -> Self {
        if !q.is_empty() {
            self.plan.filters.push(q);
        }
        self
    }

    /// Add a condition written as a string key, e.g. `("name__icontains", "ac")`.
    pub fn filter_str(self, key: &str, value: impl ToValue) -> Self {
        self.filter(Q::new(key.to_string(), value))
    }

    /// Remove rows matching the condition.
    pub fn exclude(mut self, q: Q) -> Self {
        if !q.is_empty() {
            self.plan.excludes.push(q);
        }
        self
    }

    /// Replace the ordering. Calling it again replaces rather than appends,
    /// which is what Django does and what makes the last call authoritative.
    pub fn order_by<I: IntoIterator<Item = OrderBy>>(mut self, terms: I) -> Self {
        self.plan.order = terms.into_iter().collect();
        self
    }

    /// Order by string keys, `-field` for descending.
    pub fn order_by_str(mut self, terms: &[&str]) -> Self {
        self.plan.order = terms.iter().map(|s| OrderBy::parse(s)).collect();
        self
    }

    pub fn limit(mut self, n: i64) -> Self {
        self.plan.limit = Some(n);
        self
    }

    pub fn offset(mut self, n: i64) -> Self {
        self.plan.offset = Some(n);
        self
    }

    pub fn distinct(mut self) -> Self {
        self.plan.distinct = true;
        self
    }

    /// Fetch a foreign key's row in the same query.
    pub fn select_related(mut self, field: &str) -> Self {
        self.plan.select_related.push(field.to_string());
        self
    }

    /// Add an aggregate computed per group.
    pub fn annotate(mut self, a: Aggregate) -> Self {
        self.plan.annotations.push(a);
        self
    }

    /// Project onto named columns, which may include annotation aliases.
    pub fn values(mut self, columns: &[&str]) -> Self {
        self.plan.values = columns.iter().map(|s| (*s).to_string()).collect();
        self
    }

    /// The SQL this query would run, as a **preview** — the plan as built, before
    /// tenant scoping. Kept pure (and so deterministic for snapshot tests): the
    /// ambient tenant is not read here, where no rows are returned. The execution
    /// terminals (`all`, `rows`, `count`, `update`, …) apply the tenant predicate;
    /// this shows the base query a `--sql` preview is asking to see.
    pub fn sql(&self) -> Result<Sql> {
        compiler::compile_select(&self.plan)
    }

    /// Every matching row.
    pub async fn all(self) -> Result<Vec<T>> {
        if !self.plan.values.is_empty() {
            return Err(Error::Config(
                "this query was projected with values(); call rows() instead of all()".into(),
            ));
        }
        let sql = scope::compile_select(&self.plan)?;
        let rows = run_query(&sql).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut obj = T::from_row(row, 0)?;
            // Joined rows sit after the root model's columns, in the order
            // `select_related` was called.
            let mut offset = T::meta().fields.len();
            for rel in &self.plan.select_related {
                obj.attach_related(rel, row, offset)?;
                let field = T::meta().field(rel).ok_or_else(|| Error::UnknownField {
                    model: T::meta().name.to_string(),
                    field: rel.clone(),
                })?;
                let relation = field.relation.ok_or_else(|| Error::NotARelation {
                    path: rel.clone(),
                    segment: rel.clone(),
                })?;
                offset += registry::model(relation.model)?.fields.len();
            }
            out.push(obj);
        }
        Ok(out)
    }

    /// Streams every matching row with **~constant memory** (C-F1), instead of
    /// materialising the whole result set like [`all`](Self::all).
    ///
    /// Returns a `Stream` of `Result<T>`: consume it with `tokio_stream::StreamExt`
    /// (`while let Some(row) = s.next().await { … }`) or compose it with `.map()`,
    /// `.filter()`, etc. Backpressure from a bounded channel means a 10-million-row
    /// table is read a window at a time, never allocated whole (see
    /// [`db::stream_rows`]). PostgreSQL only; a non-PG backend is refused.
    ///
    /// First cut: whole base-model rows only — `values()` projections and
    /// `select_related()` joins are rejected (the per-row join-offset decoding of
    /// [`all`](Self::all) is not yet wired into the stream).
    pub async fn stream(self) -> Result<impl tokio_stream::Stream<Item = Result<T>>> {
        if !self.plan.values.is_empty() {
            return Err(Error::Config(
                "stream() yields whole model rows; a values() projection is not supported — use rows()".into(),
            ));
        }
        if !self.plan.select_related.is_empty() {
            return Err(Error::Config(
                "stream() does not yet support select_related(); use all() for joined rows".into(),
            ));
        }
        let sql = scope::compile_select(&self.plan)?;
        let rows = db::stream_rows(&sql.text, sql.params).await?;
        // Decode each row to `T` as it arrives — the mapping is lazy, so a row is
        // built only when the consumer pulls it, preserving the constant-memory
        // profile end to end.
        Ok(tokio_stream::StreamExt::map(rows, |row| {
            row.and_then(|row| T::from_row(&row, 0))
        }))
    }

    /// The first matching row, respecting the ordering.
    pub async fn first(self) -> Result<Option<T>> {
        let qs = self.limit(1);
        Ok(qs.all().await?.into_iter().next())
    }

    /// The last matching row.
    ///
    /// Implemented by reversing the ordering and taking one, rather than by
    /// fetching everything and keeping the tail.
    pub async fn last(mut self) -> Result<Option<T>> {
        let declared: Vec<OrderBy> = T::meta()
            .ordering
            .iter()
            .map(|s| OrderBy::parse(s))
            .collect();
        let mut terms = if self.plan.order.is_empty() {
            declared
        } else {
            std::mem::take(&mut self.plan.order)
        };
        if terms.is_empty() {
            // With no ordering there is no "last"; the primary key is the only
            // stable tiebreak, and guessing differently would be arbitrary.
            let pk = T::meta().primary_key().ok_or_else(|| {
                Error::Config(format!(
                    "model `{}` has neither an ordering nor a primary key, so last() is undefined",
                    T::meta().name
                ))
            })?;
            terms = vec![OrderBy {
                path: pk.name.to_string(),
                descending: false,
            }];
        }
        for t in &mut terms {
            t.descending = !t.descending;
        }
        self.plan.order = terms;
        self.first().await
    }

    /// Exactly one row.
    pub async fn get(self) -> Result<T> {
        let model = T::meta().name.to_string();
        // Two rows are fetched, not one: that is what tells "exactly one" from
        // "more than one", which are different errors and different bugs.
        let rows = self.limit(2).all().await?;
        match rows.len() {
            0 => Err(Error::DoesNotExist { model }),
            1 => Ok(rows.into_iter().next().expect("length was just checked")),
            n => Err(Error::MultipleObjectsReturned { model, count: n }),
        }
    }

    /// How many rows match.
    pub async fn count(self) -> Result<i64> {
        let sql = scope::compile_count(&self.plan)?;
        let rows = run_query(&sql).await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::Db("COUNT returned no row".into()))?;
        row.try_get::<i64>(0)
            .map_err(|_| Error::UnexpectedColumnType {
                column: "count".into(),
                want: "bigint",
            })
    }

    /// Whether anything matches, without counting.
    pub async fn exists(self) -> Result<bool> {
        let sql = scope::compile_exists(&self.plan)?;
        let rows = run_query(&sql).await?;
        let row = rows
            .first()
            .ok_or_else(|| Error::Db("EXISTS returned no row".into()))?;
        row.try_get::<bool>(0)
            .map_err(|_| Error::UnexpectedColumnType {
                column: "exists".into(),
                want: "boolean",
            })
    }

    /// Rows as name-to-value maps. The counterpart of `values()`.
    pub async fn rows(self) -> Result<Vec<ValueRow>> {
        let sql = scope::compile_select(&self.plan)?;
        let rows = run_query(&sql).await?;
        decode(&sql, &rows)
    }

    /// Continue a keyset-paginated query from `cursor` (ADR 0024) — the opaque
    /// token [`Page::next`] handed back. The cursor is verified against this
    /// query's fingerprint, so one minted for a different query (different
    /// ordering or filters) is refused rather than paging the wrong rows.
    pub fn after(mut self, cursor: &str) -> Result<Self> {
        let boundary = cursor::decode(&keyset_fingerprint(&self.plan), cursor)?;
        self.plan.keyset = Some(boundary);
        Ok(self)
    }

    /// Fetch one keyset page of `size` rows in a single query — no `OFFSET`, so
    /// the cost is constant however deep the pagination goes (ADR 0024). The
    /// ordering is made total by a primary-key tiebreak; [`Page::next`] carries a
    /// signed cursor for the following page, or `None` at the end.
    ///
    /// Start with no `after`; continue with [`after`](Self::after).
    pub async fn fetch_page(mut self, size: i64) -> Result<Page<T>> {
        if !self.plan.values.is_empty() {
            return Err(Error::Config(
                "fetch_page() returns models; do not combine it with values()".into(),
            ));
        }
        if size <= 0 {
            return Err(Error::Config("page size must be positive".into()));
        }
        self.plan.paginate = true;
        self.plan.limit = Some(size);
        self.plan.offset = None;

        let sql = scope::compile_select(&self.plan)?;
        let rows = run_query(&sql).await?;

        // Decode each row to a model, attaching any `select_related` parents just
        // as `all()` does.
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut obj = T::from_row(row, 0)?;
            let mut offset = T::meta().fields.len();
            for rel in &self.plan.select_related {
                obj.attach_related(rel, row, offset)?;
                let field = T::meta().field(rel).ok_or_else(|| Error::UnknownField {
                    model: T::meta().name.to_string(),
                    field: rel.clone(),
                })?;
                let relation = field.relation.ok_or_else(|| Error::NotARelation {
                    path: rel.clone(),
                    segment: rel.clone(),
                })?;
                offset += registry::model(relation.model)?.fields.len();
            }
            items.push(obj);
        }

        // A full page means there may be more: mint the next cursor from the last
        // row's ordering-key values. A short page is the end.
        let next = if rows.len() as i64 == size {
            let last = rows.last().expect("a full page has a last row");
            let boundary = keyset_boundary(&self.plan, last)?;
            Some(cursor::encode(&keyset_fingerprint(&self.plan), &boundary)?)
        } else {
            None
        };
        Ok(Page { items, next })
    }

    /// Apply a set of assignments to every matching row.
    pub async fn update(self, assignments: Vec<Assignment>) -> Result<u64> {
        db::guard_write("update")?;
        let sql = scope::compile_update(&self.plan, &assignments)?;
        run_execute(&sql).await
    }

    /// Delete every matching row.
    pub async fn delete(self) -> Result<u64> {
        db::guard_write("delete")?;
        let sql = scope::compile_delete(&self.plan)?;
        run_execute(&sql).await
    }

    /// Aggregate over the whole query, with no grouping.
    pub async fn aggregate(self, aggregates: Vec<Aggregate>) -> Result<ValueRow> {
        if aggregates.is_empty() {
            return Err(Error::Config(
                "aggregate() was given nothing to compute".into(),
            ));
        }
        let sql = scope::compile_aggregate(&self.plan, &aggregates)?;
        let rows = run_query(&sql).await?;
        let mut decoded = decode(&sql, &rows)?;
        // An aggregate with no GROUP BY always returns exactly one row.
        decoded
            .pop()
            .ok_or_else(|| Error::Db("the aggregate returned no row".into()))
    }

    /// Shorthand for `aggregate` with a single `COUNT`.
    pub async fn count_of(self, path: &str) -> Result<Value> {
        let agg = Aggregate::new("count", AggKind::Count, path);
        let mut row = self.aggregate(vec![agg]).await?;
        Ok(row.remove("count").unwrap_or(Value::Null))
    }
}

/// Run a plan's `SELECT` and return its rows as name-to-value maps.
///
/// The model-erased path the dynamic front end comes in through: the `dorm`
/// shell enumerates models as `&'static ModelMeta` and has no `T` with which to
/// build a `QuerySet<T>`, yet the compiler and the row decoder need neither — a
/// `Plan` built straight from a `ModelMeta` compiles and runs identically. Safety
/// is unchanged: every identifier in the plan still came from the registry, and
/// every value is still a bound parameter.
pub async fn fetch_value_rows(plan: &Plan) -> Result<Vec<ValueRow>> {
    let sql = scope::compile_select(plan)?;
    let rows = run_query(&sql).await?;
    decode(&sql, &rows)
}

// The model-erased write path, next to `fetch_value_rows` — what the `dorm`
// shell and the `db execute` CLI run when the model is only known as a string.
// Every column name is validated against `meta` by the compiler (an unknown one
// is `Error::UnknownField`, never interpolated), every value is bound, and each
// refuses in a read-only session — so the dynamic front end keeps exactly the
// guarantees the typed one has.

/// Insert string-keyed rows and return them decoded, with database-generated
/// columns filled in (the serial key, `auto_now_add` defaults).
pub async fn insert_value_rows(
    meta: &'static crate::meta::ModelMeta,
    rows: &[Vec<(String, Value)>],
) -> Result<Vec<ValueRow>> {
    db::guard_write("insert")?;
    let sql = scope::compile_insert(meta, rows)?;
    let returned = insert_returning(meta, &sql, rows).await?;
    decode(&sql, &returned)
}

/// Update the rows `plan` selects, applying each assignment; returns the number
/// of rows changed.
pub async fn update_value_rows(plan: &Plan, assignments: &[Assignment]) -> Result<u64> {
    db::guard_write("update")?;
    let sql = scope::compile_update(plan, assignments)?;
    run_execute(&sql).await
}

/// Delete the rows `plan` selects; returns the number removed.
pub async fn delete_value_rows(plan: &Plan) -> Result<u64> {
    db::guard_write("delete")?;
    let sql = scope::compile_delete(plan)?;
    run_execute(&sql).await
}

/// Decode rows against the column plan the compiler produced.
///
/// The names and types come from the compiler, not from the server's row
/// description, so a column is read as the type the metadata declares and a
/// mismatch is an error rather than a silent reinterpretation.
fn decode(sql: &Sql, rows: &[Row]) -> Result<Vec<ValueRow>> {
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let mut map = ValueRow::new();
        for (i, spec) in sql.columns.iter().enumerate() {
            map.insert(spec.name.clone(), read_column(row, i, spec.ty, &spec.name)?);
        }
        out.push(map);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    //! The plan-building half is tested here; the compiled SQL is pinned by the
    //! snapshot tests in `tests/`, which need the example models.
    use super::*;
    use crate::meta::{FieldMeta, ModelMeta};
    use crate::value::FieldType;

    const FIELDS: &[FieldMeta] = &[FieldMeta {
        name: "id",
        column: "id",
        ty: FieldType::I64,
        nullable: false,
        primary_key: true,
        unique: false,
        index: false,
        max_length: None,
        default: None,
        auto_now_add: false,
        auto_now: false,
        relation: None,
    }];

    static META: ModelMeta = ModelMeta {
        name: "Probe",
        table: "probes",
        fields: FIELDS,
        ordering: &[],
        tenant_scoped: false,
        unique_together: &[],
        checks: &[],
    };

    #[derive(Debug, Clone)]
    struct Probe;

    impl Model for Probe {
        fn meta() -> &'static ModelMeta {
            &META
        }
        fn from_row(_row: &Row, _offset: usize) -> Result<Self> {
            Ok(Probe)
        }
        fn pk(&self) -> Value {
            Value::Null
        }
        fn column_values(&self) -> Vec<(String, Value)> {
            Vec::new()
        }
    }

    #[test]
    fn a_queryset_is_immutable_so_two_branches_cannot_disturb_each_other() {
        let base = QuerySet::<Probe>::new().filter(Q::new("id", 1i64));
        let left = base.clone().filter(Q::new("id", 2i64));
        let right = base.clone().filter(Q::new("id", 3i64));
        assert_eq!(base.plan().filters.len(), 1);
        assert_eq!(left.plan().filters.len(), 2);
        assert_eq!(right.plan().filters.len(), 2);
    }

    #[test]
    fn repeated_filters_accumulate_and_an_empty_one_is_dropped() {
        let qs = QuerySet::<Probe>::new()
            .filter(Q::new("id", 1i64))
            .filter(Q::Empty)
            .filter(Q::new("id", 2i64));
        assert_eq!(qs.plan().filters.len(), 2);
    }

    #[test]
    fn order_by_replaces_rather_than_appends() {
        let qs = QuerySet::<Probe>::new()
            .order_by_str(&["id"])
            .order_by_str(&["-id"]);
        assert_eq!(qs.plan().order.len(), 1);
        assert!(qs.plan().order[0].descending);
    }

    #[test]
    fn exclude_and_filter_are_kept_apart_in_the_plan() {
        let qs = QuerySet::<Probe>::new()
            .filter(Q::new("id", 1i64))
            .exclude(Q::new("id", 2i64));
        assert_eq!(qs.plan().filters.len(), 1);
        assert_eq!(qs.plan().excludes.len(), 1);
    }

    #[test]
    fn values_and_all_are_mutually_exclusive() {
        // Calling `all()` on a projected query would decode the wrong columns
        // into the struct, so it is refused rather than attempted.
        let qs = QuerySet::<Probe>::new().values(&["id"]);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime");
        let err = rt.block_on(qs.all()).unwrap_err();
        assert!(err.to_string().contains("values()"), "{err}");
    }
}
