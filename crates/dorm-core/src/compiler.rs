//! The query compiler: a plan in, parameterised SQL out. No I/O lives here.
//!
//! Being pure is what makes it testable with snapshots, and a snapshot is the
//! right instrument because the two things a refactor breaks silently are the
//! generated text and the *order of the parameters*. A unit test asserting
//! "contains ILIKE" would pass while binding `$2` where `$1` was meant.
//!
//! Three correctness decisions are worth reading before the code:
//!
//! 1. **The `%` of a `contains` goes in the value, never in the SQL.** The text
//!    is `ILIKE $1` and the parameter is `%ac%`. The alternative — splicing the
//!    pattern into the statement — is the injection this whole design exists to
//!    make impossible.
//! 2. **`LIKE` metacharacters in the value are escaped.** Without this,
//!    `name__contains = "50%"` matches every row, and `"a_b"` matches `"axb"`.
//!    Searching for a literal percent sign is not an exotic request.
//! 3. **A filter on an annotation becomes `HAVING`, not `WHERE`.** `filter(#{
//!    n__gte: 1 })` after `annotate(#{ n: Count("apps") })` cannot be a `WHERE`
//!    clause: the aggregate does not exist yet when `WHERE` is evaluated.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::error::{Error, Result};
use crate::expr::{Aggregate, Assignment, OrderBy, Predicate, F, Q};
use crate::lookup::Lookup;
use crate::meta::ModelMeta;
use crate::registry::{self, Join, Resolved};
use crate::value::{FieldType, Value};

/// One column of a result set, with the type needed to decode it dynamically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSpec {
    pub name: String,
    pub ty: FieldType,
}

/// Compiled SQL, its parameters in binding order, and how to read the result.
#[derive(Debug, Clone, PartialEq)]
pub struct Sql {
    pub text: String,
    pub params: Vec<Value>,
    pub columns: Vec<ColumnSpec>,
}

/// What a `QuerySet` accumulates, and all the compiler needs.
///
/// Kept as plain data so a plan can be built by the typed API, by the REPL, or
/// by a test, and compiled identically.
#[derive(Debug, Clone)]
pub struct Plan {
    pub model: &'static ModelMeta,
    /// `AND`ed together.
    pub filters: Vec<Q>,
    /// `AND`ed together, each negated — which is what `exclude` means.
    pub excludes: Vec<Q>,
    /// Empty means fall back to the model's declared `ordering`.
    pub order: Vec<OrderBy>,
    /// Suppress ordering entirely, declared ordering included.
    ///
    /// Needed because an empty `order` means "the caller did not ask", which
    /// falls back to the model's `ordering` — correct for a `SELECT`, wrong
    /// inside the `WHERE pk IN (…)` subquery of an `UPDATE`, where it would
    /// force a sort whose result is then discarded.
    pub unordered: bool,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub distinct: bool,
    /// Foreign key field names to join and decode alongside the root row.
    pub select_related: Vec<String>,
    pub annotations: Vec<Aggregate>,
    /// Explicit output columns: field paths, or annotation aliases.
    pub values: Vec<String>,
}

impl Plan {
    pub fn new(model: &'static ModelMeta) -> Plan {
        Plan {
            model,
            filters: Vec::new(),
            excludes: Vec::new(),
            order: Vec::new(),
            unordered: false,
            limit: None,
            offset: None,
            distinct: false,
            select_related: Vec::new(),
            annotations: Vec::new(),
            values: Vec::new(),
        }
    }

    fn annotation(&self, alias: &str) -> Option<&Aggregate> {
        self.annotations.iter().find(|a| a.alias == alias)
    }
}

/// Quote an identifier for PostgreSQL.
///
/// Every identifier reaching this function came from `&'static` metadata emitted
/// by the derive macro, so the error below is unreachable from user input by
/// construction. It is still checked: "unreachable by construction" is a claim
/// that stops being true the moment someone adds a code path, and the cost of
/// being wrong here is arbitrary SQL.
fn quote(ident: &str) -> Result<String> {
    if ident.is_empty() || ident.contains('"') || ident.contains('\0') {
        return Err(Error::Config(format!(
            "identifier `{ident}` is not a usable SQL name"
        )));
    }
    Ok(format!("\"{ident}\""))
}

/// `"alias"."column"`.
fn qualified(alias: &str, column: &str) -> Result<String> {
    Ok(format!("{}.{}", quote(alias)?, quote(column)?))
}

/// Escape the `LIKE`/`ILIKE` metacharacters in a pattern operand.
///
/// Backslash first, or the escapes added afterwards would themselves be escaped.
fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

/// The SQL dialect a plan is compiled for. Only the placeholder style (`$n` vs
/// `?n`) and case-insensitive `LIKE` differ between the two today; identifiers,
/// joins and clause structure are shared (ADR 0002). PostgreSQL is the default,
/// so every existing caller and every snapshot is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Postgres,
    Sqlite,
}

impl Dialect {
    /// The case-insensitive pattern operator: PostgreSQL's `ILIKE`, or `LIKE` on
    /// SQLite (whose `LIKE` is already case-insensitive for ASCII).
    fn ilike(self) -> &'static str {
        match self {
            Dialect::Postgres => "ILIKE",
            Dialect::Sqlite => "LIKE",
        }
    }
}

/// Accumulates parameters so the placeholder numbers and the vector stay in step.
struct Binder {
    params: Vec<Value>,
    dialect: Dialect,
}

impl Binder {
    fn new() -> Binder {
        Binder::with_dialect(Dialect::Postgres)
    }

    fn with_dialect(dialect: Dialect) -> Binder {
        Binder {
            params: Vec::new(),
            dialect,
        }
    }

    /// Bind one value and return its placeholder — `$n` on PostgreSQL, `?n` on
    /// SQLite.
    fn bind(&mut self, v: Value) -> String {
        self.params.push(v);
        match self.dialect {
            Dialect::Postgres => format!("${}", self.params.len()),
            Dialect::Sqlite => format!("?{}", self.params.len()),
        }
    }
}

/// Joins gathered during resolution, deduplicated and kept in a stable order.
///
/// `BTreeMap` rather than `HashMap`: the iteration order of the joins ends up in
/// the generated SQL, and a snapshot test over a `HashMap`'s order is a flake
/// waiting for a rehash.
#[derive(Default)]
struct Joins(BTreeMap<String, Join>);

impl Joins {
    fn absorb(&mut self, joins: Vec<Join>) {
        for j in joins {
            self.0.entry(j.alias.clone()).or_insert(j);
        }
    }

    fn emit(&self) -> Result<String> {
        let mut out = String::new();
        for j in self.0.values() {
            write!(
                out,
                " LEFT JOIN {} AS {} ON {} = {}",
                quote(j.table)?,
                quote(&j.alias)?,
                qualified(&j.from_alias, j.from_column)?,
                qualified(&j.alias, j.to_column)?
            )
            .map_err(|e| Error::Config(e.to_string()))?;
        }
        Ok(out)
    }
}

/// Where a resolved condition belongs.
enum Clause {
    Where(String),
    Having(String),
}

/// Compile one predicate into SQL, binding its values.
fn compile_predicate(
    plan: &Plan,
    p: &Predicate,
    binder: &mut Binder,
    joins: &mut Joins,
) -> Result<Clause> {
    // An annotation alias shadows a field: after `annotate(#{ n: ... })`, `n__gte`
    // is a condition on the aggregate, and it has to land in HAVING.
    let (head, tail) = match p.path.split_once("__") {
        Some((h, t)) => (h, Some(t)),
        None => (p.path.as_str(), None),
    };
    if let Some(agg) = plan.annotation(head) {
        let lookup = match tail {
            None => Lookup::Exact,
            Some(t) => Lookup::parse(t).ok_or_else(|| Error::UnknownLookup {
                lookup: t.to_string(),
                path: p.path.clone(),
            })?,
        };
        let ty = agg.kind.result_type().unwrap_or(FieldType::I64);
        lookup.validate(ty, &p.path, &p.values)?;
        let operand = compile_aggregate_expr(plan, agg, joins)?;
        return Ok(Clause::Having(compile_comparison(
            &operand, lookup, ty, &p.values, binder,
        )?));
    }

    let Resolved {
        joins: path_joins,
        alias,
        field,
        lookup,
    } = registry::resolve(plan.model, &p.path)?;
    joins.absorb(path_joins);

    // A foreign key compared without traversing it is compared on its own
    // column, and the value is the referenced primary key.
    let ty = field.ty;
    lookup.validate(ty, &p.path, &p.values)?;

    let operand = qualified(&alias, field.column)?;
    let values = if lookup == Lookup::IsNull {
        // The boolean of an `isnull` belongs to the operator — it picks between
        // `IS NULL` and `IS NOT NULL` — and is never bound to the column.
        // Coercing it towards the column's type would reject
        // `name__isnull = true` on a text column, which is a legitimate query.
        p.values.clone()
    } else {
        p.values
            .iter()
            .cloned()
            .map(|v| v.coerce(ty, field.column))
            .collect::<Result<Vec<_>>>()?
    };
    Ok(Clause::Where(compile_comparison(
        &operand, lookup, ty, &values, binder,
    )?))
}

/// The comparison itself, shared by the `WHERE` and `HAVING` paths.
fn compile_comparison(
    operand: &str,
    lookup: Lookup,
    ty: FieldType,
    values: &[Value],
    binder: &mut Binder,
) -> Result<String> {
    let first = || values[0].clone();
    Ok(match lookup {
        Lookup::Exact => {
            // `= NULL` is never true; a caller writing `.eq(None)` means IS NULL.
            if values[0].is_null() {
                format!("{operand} IS NULL")
            } else {
                format!("{operand} = {}", binder.bind(first()))
            }
        }
        Lookup::IExact => {
            let v = pattern(&values[0], ty, PatternShape::Exact)?;
            let op = binder.dialect.ilike();
            format!("{operand} {op} {}", binder.bind(v))
        }
        Lookup::Contains => {
            let v = pattern(&values[0], ty, PatternShape::Anywhere)?;
            format!("{operand} LIKE {}", binder.bind(v))
        }
        Lookup::IContains => {
            let v = pattern(&values[0], ty, PatternShape::Anywhere)?;
            let op = binder.dialect.ilike();
            format!("{operand} {op} {}", binder.bind(v))
        }
        Lookup::StartsWith => {
            let v = pattern(&values[0], ty, PatternShape::Prefix)?;
            format!("{operand} LIKE {}", binder.bind(v))
        }
        // `endswith` concatenates the wildcard in SQL rather than baking it into
        // the value: the parameter is the escaped suffix and the `%` is a literal
        // in the statement. `… LIKE '%' || $1` reads as "ends with", and the
        // escaping still happens so a suffix containing `%` or `_` is matched
        // literally, not as a wildcard.
        Lookup::EndsWith => {
            let v = pattern(&values[0], ty, PatternShape::Exact)?;
            format!("{operand} LIKE '%' || {}", binder.bind(v))
        }
        Lookup::IEndsWith => {
            let v = pattern(&values[0], ty, PatternShape::Exact)?;
            let op = binder.dialect.ilike();
            format!("{operand} {op} '%' || {}", binder.bind(v))
        }
        // The regex operators take a POSIX pattern, so the value is bound raw —
        // its metacharacters are the point, and escaping them as a `LIKE` pattern
        // would defeat the lookup. `~` is case-sensitive, `~*` case-insensitive.
        Lookup::Regex => format!("{operand} ~ {}", binder.bind(first())),
        Lookup::IRegex => format!("{operand} ~* {}", binder.bind(first())),
        Lookup::Gt => format!("{operand} > {}", binder.bind(first())),
        Lookup::Gte => format!("{operand} >= {}", binder.bind(first())),
        Lookup::Lt => format!("{operand} < {}", binder.bind(first())),
        Lookup::Lte => format!("{operand} <= {}", binder.bind(first())),
        Lookup::In => {
            // Expanded to one placeholder per element rather than `= ANY($1)`,
            // so every parameter stays a scalar and no array type has to be
            // inferred for a list the REPL just assembled.
            let placeholders = values
                .iter()
                .cloned()
                .map(|v| binder.bind(v))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{operand} IN ({placeholders})")
        }
        Lookup::Range => format!(
            "{operand} BETWEEN {} AND {}",
            binder.bind(values[0].clone()),
            binder.bind(values[1].clone())
        ),
        Lookup::IsNull => {
            // The boolean is part of the operator, not a bound value: `IS NULL`
            // and `IS NOT NULL` are different SQL, and `isnull` was already
            // validated to carry a boolean.
            match values[0] {
                Value::Bool(true) => format!("{operand} IS NULL"),
                _ => format!("{operand} IS NOT NULL"),
            }
        }
    })
}

enum PatternShape {
    Exact,
    Anywhere,
    Prefix,
}

/// Build a `LIKE` operand, escaped, with the wildcards this lookup wants.
fn pattern(v: &Value, ty: FieldType, shape: PatternShape) -> Result<Value> {
    let s = v.as_text().ok_or(Error::UnexpectedColumnType {
        column: ty.describe().to_string(),
        want: "text, for a pattern lookup",
    })?;
    let e = escape_like(s);
    Ok(Value::Text(match shape {
        PatternShape::Exact => e,
        PatternShape::Anywhere => format!("%{e}%"),
        PatternShape::Prefix => format!("{e}%"),
    }))
}

/// Compile a condition tree.
///
/// `WHERE` and `HAVING` parts are kept apart and recombined by the caller. A
/// tree that mixes the two inside one `OR` is refused rather than silently
/// split, because `(aggregate OR column)` cannot be expressed as one of each.
fn compile_q(
    plan: &Plan,
    q: &Q,
    binder: &mut Binder,
    joins: &mut Joins,
) -> Result<(Option<String>, Option<String>)> {
    match q {
        Q::Empty => Ok((None, None)),
        Q::Pred(p) => match compile_predicate(plan, p, binder, joins)? {
            Clause::Where(s) => Ok((Some(s), None)),
            Clause::Having(s) => Ok((None, Some(s))),
        },
        Q::And(a, b) => {
            let (wa, ha) = compile_q(plan, a, binder, joins)?;
            let (wb, hb) = compile_q(plan, b, binder, joins)?;
            Ok((combine(wa, wb, "AND"), combine(ha, hb, "AND")))
        }
        Q::Or(a, b) => {
            let (wa, ha) = compile_q(plan, a, binder, joins)?;
            let (wb, hb) = compile_q(plan, b, binder, joins)?;
            // An OR with an aggregate on one side and a column on the other
            // would have to be split across WHERE and HAVING, which changes its
            // meaning. Refuse it instead.
            if (wa.is_some() && hb.is_some()) || (ha.is_some() && wb.is_some()) {
                return Err(Error::Config(
                    "an `or` cannot mix an aggregate with a plain column; \
                     filter the aggregate separately"
                        .into(),
                ));
            }
            Ok((combine(wa, wb, "OR"), combine(ha, hb, "OR")))
        }
        Q::Not(inner) => {
            let (w, h) = compile_q(plan, inner, binder, joins)?;
            Ok((
                w.map(|s| format!("NOT ({s})")),
                h.map(|s| format!("NOT ({s})")),
            ))
        }
    }
}

fn combine(a: Option<String>, b: Option<String>, op: &str) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(format!("({a} {op} {b})")),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// The `WHERE` and `HAVING` text for a whole plan, filters and excludes together.
fn compile_conditions(
    plan: &Plan,
    binder: &mut Binder,
    joins: &mut Joins,
) -> Result<(Option<String>, Option<String>)> {
    let mut wheres: Vec<String> = Vec::new();
    let mut havings: Vec<String> = Vec::new();

    for q in &plan.filters {
        let (w, h) = compile_q(plan, q, binder, joins)?;
        wheres.extend(w);
        havings.extend(h);
    }
    for q in &plan.excludes {
        // `exclude` is `NOT (…)` over the whole condition, not a negation of
        // each leaf: excluding `a AND b` must keep rows that fail only `b`.
        let (w, h) = compile_q(plan, q, binder, joins)?;
        wheres.extend(w.map(|s| format!("NOT ({s})")));
        havings.extend(h.map(|s| format!("NOT ({s})")));
    }

    let join_all = |v: Vec<String>| {
        if v.is_empty() {
            None
        } else {
            Some(v.join(" AND "))
        }
    };
    Ok((join_all(wheres), join_all(havings)))
}

/// The SQL for one aggregate, resolving a `related_name` into a join when the
/// aggregate counts the far side of a relation.
fn compile_aggregate_expr(plan: &Plan, agg: &Aggregate, joins: &mut Joins) -> Result<String> {
    // `Count("apps")` on Tenant: `apps` is a related_name, so the thing being
    // counted lives in another table and needs a join before it can be counted.
    if let Some((child, fk)) = registry::reverse_relation(plan.model.name, &agg.path) {
        let pk = plan.model.primary_key().ok_or_else(|| {
            Error::Config(format!(
                "model `{}` needs a primary key to aggregate over `{}`",
                plan.model.name, agg.path
            ))
        })?;
        let alias = format!("r_{}", agg.path);
        joins.absorb(vec![Join {
            table: child.table,
            alias: alias.clone(),
            from_alias: registry::root_alias(plan.model),
            from_column: pk.column,
            to_column: fk.column,
            nullable: true,
        }]);
        let target = child.primary_key().map_or(fk.column, |p| p.column);
        // DISTINCT matters: with a second join in the query, a plain COUNT
        // multiplies by the other join's fan-out.
        return Ok(format!(
            "{}(DISTINCT {})",
            agg.kind.sql(),
            qualified(&alias, target)?
        ));
    }

    let Resolved {
        joins: path_joins,
        alias,
        field,
        ..
    } = registry::resolve(plan.model, &agg.path)?;
    joins.absorb(path_joins);
    Ok(format!(
        "{}({})",
        agg.kind.sql(),
        qualified(&alias, field.column)?
    ))
}

/// The result type of an aggregate, for decoding.
fn aggregate_type(plan: &Plan, agg: &Aggregate) -> FieldType {
    if let Some(t) = agg.kind.result_type() {
        return t;
    }
    registry::resolve(plan.model, &agg.path)
        .map(|r| r.field.ty)
        .unwrap_or(FieldType::I64)
}

/// `SELECT` for a plan: the root columns, anything `select_related` brought in,
/// and the annotations.
fn compile_select_list(plan: &Plan, joins: &mut Joins) -> Result<(String, Vec<ColumnSpec>)> {
    let mut parts: Vec<String> = Vec::new();
    let mut specs: Vec<ColumnSpec> = Vec::new();
    let root = registry::root_alias(plan.model);

    if plan.values.is_empty() {
        // Columns are always named, never `*`. A `SELECT *` makes the decoder
        // depend on physical column order — the defect that already exists at
        // `crates/delonix-api/src/cpstore/store_api_keys.rs:42`.
        for f in plan.model.fields {
            parts.push(qualified(&root, f.column)?);
            specs.push(ColumnSpec {
                name: f.name.to_string(),
                ty: f.ty,
            });
        }
        for rel in &plan.select_related {
            let field = plan.model.field(rel).ok_or_else(|| Error::UnknownField {
                model: plan.model.name.to_string(),
                field: rel.clone(),
            })?;
            let relation = field.relation.ok_or_else(|| Error::NotARelation {
                path: rel.clone(),
                segment: rel.clone(),
            })?;
            let target = registry::model(relation.model)?;
            let pk = target.primary_key().ok_or_else(|| {
                Error::Config(format!("model `{}` declares no primary key", target.name))
            })?;
            let alias = format!("j_{rel}");
            joins.absorb(vec![Join {
                table: target.table,
                alias: alias.clone(),
                from_alias: root.clone(),
                from_column: field.column,
                to_column: pk.column,
                nullable: field.nullable,
            }]);
            for f in target.fields {
                parts.push(qualified(&alias, f.column)?);
                specs.push(ColumnSpec {
                    name: format!("{rel}__{}", f.name),
                    ty: f.ty,
                });
            }
        }
    } else {
        for name in &plan.values {
            if let Some(agg) = plan.annotation(name) {
                parts.push(format!(
                    "{} AS {}",
                    compile_aggregate_expr(plan, agg, joins)?,
                    quote(&agg.alias)?
                ));
                specs.push(ColumnSpec {
                    name: agg.alias.clone(),
                    ty: aggregate_type(plan, agg),
                });
                continue;
            }
            let r = registry::resolve(plan.model, name)?;
            joins.absorb(r.joins);
            parts.push(qualified(&r.alias, r.field.column)?);
            specs.push(ColumnSpec {
                name: name.clone(),
                ty: r.field.ty,
            });
        }
    }

    // Annotations not already named by `values` are appended, so `annotate`
    // followed by no `values` still returns them.
    for agg in &plan.annotations {
        if specs.iter().any(|s| s.name == agg.alias) {
            continue;
        }
        parts.push(format!(
            "{} AS {}",
            compile_aggregate_expr(plan, agg, joins)?,
            quote(&agg.alias)?
        ));
        specs.push(ColumnSpec {
            name: agg.alias.clone(),
            ty: aggregate_type(plan, agg),
        });
    }

    Ok((parts.join(", "), specs))
}

/// `ORDER BY`, falling back to the model's declared ordering.
fn compile_order(plan: &Plan, joins: &mut Joins) -> Result<Option<String>> {
    if plan.unordered {
        return Ok(None);
    }
    let declared: Vec<OrderBy> = plan
        .model
        .ordering
        .iter()
        .map(|s| OrderBy::parse(s))
        .collect();
    let terms = if plan.order.is_empty() {
        &declared
    } else {
        &plan.order
    };
    if terms.is_empty() {
        return Ok(None);
    }

    let mut parts = Vec::new();
    for t in terms {
        let direction = if t.descending { "DESC" } else { "ASC" };
        if let Some(agg) = plan.annotation(&t.path) {
            parts.push(format!("{} {direction}", quote(&agg.alias)?));
            continue;
        }
        let r = registry::resolve(plan.model, &t.path)?;
        joins.absorb(r.joins);
        parts.push(format!(
            "{} {direction}",
            qualified(&r.alias, r.field.column)?
        ));
    }
    Ok(Some(parts.join(", ")))
}

/// Compile a `SELECT` for PostgreSQL. See [`compile_select_on`] for SQLite.
pub fn compile_select(plan: &Plan) -> Result<Sql> {
    compile_select_on(plan, Dialect::Postgres)
}

/// Compile a `SELECT` for `dialect`. Only the placeholders and case-insensitive
/// `LIKE` differ; the shape is identical.
pub fn compile_select_on(plan: &Plan, dialect: Dialect) -> Result<Sql> {
    let mut binder = Binder::with_dialect(dialect);
    let mut joins = Joins::default();

    // Order of work, not order of text: the clauses that bind parameters are
    // compiled in the order they will appear, so placeholder numbers match.
    let (select_list, columns) = compile_select_list(plan, &mut joins)?;
    let (where_clause, having_clause) = compile_conditions(plan, &mut binder, &mut joins)?;
    let order_clause = compile_order(plan, &mut joins)?;

    let mut text = String::from("SELECT ");
    if plan.distinct {
        text.push_str("DISTINCT ");
    }
    text.push_str(&select_list);
    text.push_str(" FROM ");
    text.push_str(&quote(plan.model.table)?);
    text.push_str(&joins.emit()?);
    if let Some(w) = &where_clause {
        text.push_str(" WHERE ");
        text.push_str(w);
    }
    if !plan.annotations.is_empty() {
        // Grouping by the primary key alone is enough: PostgreSQL recognises the
        // functional dependency and allows any other column of that table in the
        // select list, so the group stays one row per model instance.
        let pk = plan.model.primary_key().ok_or_else(|| {
            Error::Config(format!(
                "model `{}` needs a primary key to be annotated",
                plan.model.name
            ))
        })?;
        text.push_str(" GROUP BY ");
        text.push_str(&qualified(&registry::root_alias(plan.model), pk.column)?);
    }
    if let Some(h) = &having_clause {
        text.push_str(" HAVING ");
        text.push_str(h);
    }
    if let Some(o) = order_clause {
        text.push_str(" ORDER BY ");
        text.push_str(&o);
    }
    // `limit` and `offset` are `i64`, not caller text, so writing them inline is
    // safe and keeps the snapshots readable.
    if let Some(n) = plan.limit {
        write!(text, " LIMIT {n}").map_err(|e| Error::Config(e.to_string()))?;
    }
    if let Some(n) = plan.offset {
        write!(text, " OFFSET {n}").map_err(|e| Error::Config(e.to_string()))?;
    }

    Ok(Sql {
        text,
        params: binder.params,
        columns,
    })
}

/// Compile a `COUNT(*)` over the same conditions.
pub fn compile_count(plan: &Plan) -> Result<Sql> {
    // An annotated or distinct plan has to be counted as a subquery: counting
    // the grouped rows is the question being asked, and `COUNT(*)` over the
    // groups would answer a different one.
    if !plan.annotations.is_empty()
        || plan.distinct
        || plan.limit.is_some()
        || plan.offset.is_some()
    {
        let inner = compile_select(plan)?;
        return Ok(Sql {
            text: format!("SELECT COUNT(*) FROM ({}) AS _c", inner.text),
            params: inner.params,
            columns: vec![ColumnSpec {
                name: "count".into(),
                ty: FieldType::I64,
            }],
        });
    }

    let mut binder = Binder::new();
    let mut joins = Joins::default();
    let (where_clause, _) = compile_conditions(plan, &mut binder, &mut joins)?;

    let mut text = String::from("SELECT COUNT(*) FROM ");
    text.push_str(&quote(plan.model.table)?);
    text.push_str(&joins.emit()?);
    if let Some(w) = where_clause {
        text.push_str(" WHERE ");
        text.push_str(&w);
    }
    Ok(Sql {
        text,
        params: binder.params,
        columns: vec![ColumnSpec {
            name: "count".into(),
            ty: FieldType::I64,
        }],
    })
}

/// Compile `EXISTS`, which must not pay for counting rows it will not report.
pub fn compile_exists(plan: &Plan) -> Result<Sql> {
    let mut probe = plan.clone();
    probe.unordered = true;
    probe.limit = Some(1);
    probe.values = vec![];
    let inner = compile_select(&probe)?;
    Ok(Sql {
        text: format!("SELECT EXISTS (SELECT 1 FROM ({}) AS _e)", inner.text),
        params: inner.params,
        columns: vec![ColumnSpec {
            name: "exists".into(),
            ty: FieldType::Bool,
        }],
    })
}

/// Compile an `aggregate()` call — aggregates with no grouping.
pub fn compile_aggregate(plan: &Plan, aggs: &[Aggregate]) -> Result<Sql> {
    let mut binder = Binder::new();
    let mut joins = Joins::default();

    let mut parts = Vec::new();
    let mut columns = Vec::new();
    for agg in aggs {
        parts.push(format!(
            "{} AS {}",
            compile_aggregate_expr(plan, agg, &mut joins)?,
            quote(&agg.alias)?
        ));
        columns.push(ColumnSpec {
            name: agg.alias.clone(),
            ty: aggregate_type(plan, agg),
        });
    }
    let (where_clause, _) = compile_conditions(plan, &mut binder, &mut joins)?;

    let mut text = format!(
        "SELECT {} FROM {}",
        parts.join(", "),
        quote(plan.model.table)?
    );
    text.push_str(&joins.emit()?);
    if let Some(w) = where_clause {
        text.push_str(" WHERE ");
        text.push_str(&w);
    }
    Ok(Sql {
        text,
        params: binder.params,
        columns,
    })
}

/// Render an `F` expression, binding its literals.
fn compile_f(plan: &Plan, e: &F, binder: &mut Binder) -> Result<String> {
    Ok(match e {
        F::Col(name) => {
            let field = plan.model.field(name).ok_or_else(|| Error::UnknownField {
                model: plan.model.name.to_string(),
                field: name.clone(),
            })?;
            // Unqualified: an UPDATE's SET clause refers to the target table's
            // own columns, and qualifying them is a syntax error in PostgreSQL.
            quote(field.column)?
        }
        F::Lit(v) => binder.bind(v.clone()),
        F::Add(a, b) => format!(
            "({} + {})",
            compile_f(plan, a, binder)?,
            compile_f(plan, b, binder)?
        ),
        F::Sub(a, b) => format!(
            "({} - {})",
            compile_f(plan, a, binder)?,
            compile_f(plan, b, binder)?
        ),
        F::Mul(a, b) => format!(
            "({} * {})",
            compile_f(plan, a, binder)?,
            compile_f(plan, b, binder)?
        ),
        F::Div(a, b) => format!(
            "({} / {})",
            compile_f(plan, a, binder)?,
            compile_f(plan, b, binder)?
        ),
    })
}

/// Compile an `UPDATE`.
///
/// A plan whose conditions traverse a relation cannot become a bare `UPDATE …
/// WHERE`, because an `UPDATE` has no join. Those compile to `WHERE pk IN
/// (subquery)` instead, which is both correct and the only form that keeps
/// `update()` meaningful after `filter(tenant__slug=…)`.
pub fn compile_update(plan: &Plan, assignments: &[Assignment]) -> Result<Sql> {
    if assignments.is_empty() {
        return Err(Error::Config("update() was given nothing to set".into()));
    }
    let pk = plan.model.primary_key().ok_or_else(|| {
        Error::Config(format!(
            "model `{}` declares no primary key, so it cannot be updated by query",
            plan.model.name
        ))
    })?;

    let mut binder = Binder::new();
    let mut sets = Vec::new();
    for a in assignments {
        let field = plan
            .model
            .field(&a.field)
            .ok_or_else(|| Error::UnknownField {
                model: plan.model.name.to_string(),
                field: a.field.clone(),
            })?;
        sets.push(format!(
            "{} = {}",
            quote(field.column)?,
            compile_f(plan, &a.expr, &mut binder)?
        ));
    }
    // `auto_now` columns are refreshed on every write without the caller asking.
    for f in plan.model.fields {
        if f.auto_now && !assignments.iter().any(|a| a.field == f.name) {
            sets.push(format!("{} = now()", quote(f.column)?));
        }
    }

    let mut selector = plan.clone();
    selector.unordered = true;
    selector.values = vec![pk.name.to_string()];
    let inner = compile_select(&selector)?;

    let mut text = format!(
        "UPDATE {} SET {}",
        quote(plan.model.table)?,
        sets.join(", ")
    );
    // The subquery's own placeholders have to be renumbered to sit after the SET
    // clause's. Done by compiling the selector against a binder that already
    // holds the SET parameters.
    let shifted = shift_placeholders(&inner.text, binder.params.len());
    write!(text, " WHERE {} IN ({})", quote(pk.column)?, shifted)
        .map_err(|e| Error::Config(e.to_string()))?;
    let mut params = binder.params;
    params.extend(inner.params);

    Ok(Sql {
        text,
        params,
        columns: vec![],
    })
}

/// Compile a `DELETE`, by the same subquery route as `compile_update`.
pub fn compile_delete(plan: &Plan) -> Result<Sql> {
    let pk = plan.model.primary_key().ok_or_else(|| {
        Error::Config(format!(
            "model `{}` declares no primary key, so it cannot be deleted by query",
            plan.model.name
        ))
    })?;
    let mut selector = plan.clone();
    selector.unordered = true;
    selector.values = vec![pk.name.to_string()];
    let inner = compile_select(&selector)?;

    Ok(Sql {
        text: format!(
            "DELETE FROM {} WHERE {} IN ({})",
            quote(plan.model.table)?,
            quote(pk.column)?,
            inner.text
        ),
        params: inner.params,
        columns: vec![],
    })
}

/// Renumber `$n` placeholders so a compiled fragment can be spliced after
/// parameters that were bound before it.
///
/// Scanning for `$` is safe here because the only text this is ever applied to
/// is compiler output, where every literal has already become a placeholder —
/// there is no string constant left that could contain a `$`.
fn shift_placeholders(sql: &str, by: usize) -> String {
    if by == 0 {
        return sql.to_string();
    }
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '$' {
            out.push(c);
            continue;
        }
        let mut digits = String::new();
        while let Some(d) = chars.peek().filter(|d| d.is_ascii_digit()) {
            digits.push(*d);
            chars.next();
        }
        match digits.parse::<usize>() {
            Ok(n) => {
                out.push('$');
                out.push_str(&(n + by).to_string());
            }
            Err(_) => {
                out.push('$');
                out.push_str(&digits);
            }
        }
    }
    out
}

/// Compile an `INSERT` of one or more rows.
///
/// Every row must offer the same columns, which `bulk_create` guarantees by
/// construction and `create` trivially satisfies.
pub fn compile_insert(meta: &'static ModelMeta, rows: &[Vec<(String, Value)>]) -> Result<Sql> {
    compile_insert_on(meta, rows, Dialect::Postgres)
}

/// Compile an `INSERT … RETURNING *` for `dialect`.
pub fn compile_insert_on(
    meta: &'static ModelMeta,
    rows: &[Vec<(String, Value)>],
    dialect: Dialect,
) -> Result<Sql> {
    let first = rows
        .first()
        .ok_or_else(|| Error::Config("insert was given no rows".into()))?;

    let mut fields = Vec::new();
    for (name, _) in first {
        let f = meta.field(name).ok_or_else(|| Error::UnknownField {
            model: meta.name.to_string(),
            field: name.clone(),
        })?;
        fields.push(f);
    }

    let mut binder = Binder::with_dialect(dialect);
    let mut tuples = Vec::new();
    for row in rows {
        if row.len() != first.len() {
            return Err(Error::Config(
                "every row of a bulk insert must set the same columns".into(),
            ));
        }
        let mut placeholders = Vec::new();
        for (f, (name, value)) in fields.iter().zip(row) {
            if f.name != name.as_str() {
                return Err(Error::Config(format!(
                    "bulk insert column order differs: expected `{}`, got `{name}`",
                    f.name
                )));
            }
            placeholders.push(binder.bind(value.clone().coerce(f.ty, f.column)?));
        }
        tuples.push(format!("({})", placeholders.join(", ")));
    }

    let columns = fields
        .iter()
        .map(|f| quote(f.column))
        .collect::<Result<Vec<_>>>()?
        .join(", ");
    // Everything comes back, so the caller gets database-generated columns —
    // the serial primary key, `auto_now_add` defaults — without a second query.
    let returning = meta
        .columns()
        .map(quote)
        .collect::<Result<Vec<_>>>()?
        .join(", ");

    Ok(Sql {
        text: format!(
            "INSERT INTO {} ({columns}) VALUES {} RETURNING {returning}",
            quote(meta.table)?,
            tuples.join(", ")
        ),
        params: binder.params,
        columns: meta
            .fields
            .iter()
            .map(|f| ColumnSpec {
                name: f.name.to_string(),
                ty: f.ty,
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_metacharacters_in_the_value_are_escaped() {
        // Without this, `contains("50%")` matches every row.
        assert_eq!(escape_like("50%"), "50\\%");
        assert_eq!(escape_like("a_b"), "a\\_b");
        // The backslash is escaped first, so the escapes are not re-escaped.
        assert_eq!(escape_like("a\\b"), "a\\\\b");
        assert_eq!(escape_like("plain"), "plain");
    }

    #[test]
    fn an_identifier_that_could_break_out_of_its_quotes_is_refused() {
        assert!(quote("slug").is_ok());
        assert!(quote("a\"b").is_err());
        assert!(quote("").is_err());
        assert!(quote("a\0b").is_err());
    }

    #[test]
    fn placeholders_are_renumbered_when_a_fragment_is_spliced_after_others() {
        assert_eq!(
            shift_placeholders("a = $1 AND b = $2", 2),
            "a = $3 AND b = $4"
        );
        // Double digits must not be split into `$1` plus a stray `0`.
        assert_eq!(shift_placeholders("x = $10", 1), "x = $11");
        assert_eq!(shift_placeholders("no params", 3), "no params");
        assert_eq!(shift_placeholders("a = $1", 0), "a = $1");
    }

    #[test]
    fn a_binder_keeps_placeholder_numbers_and_the_parameter_vector_in_step() {
        let mut b = Binder::new();
        assert_eq!(b.bind(Value::I64(1)), "$1");
        assert_eq!(b.bind(Value::Text("x".into())), "$2");
        assert_eq!(b.params, vec![Value::I64(1), Value::Text("x".into())]);
    }

    #[test]
    fn conditions_are_combined_without_a_dangling_operator() {
        assert_eq!(
            combine(Some("a".into()), Some("b".into()), "AND"),
            Some("(a AND b)".into())
        );
        assert_eq!(combine(Some("a".into()), None, "AND"), Some("a".into()));
        assert_eq!(combine(None, None, "AND"), None);
    }
}
