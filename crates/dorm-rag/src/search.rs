//! Semantic and hybrid search (ADR 0019, brick 5), in both front ends.
//!
//! * Typed: [`RagQuerySetExt::semantic`] extends `QuerySet<T>`, so
//!   `Ticket::objects().filter(..).semantic("..").hybrid(0.7).search()` reads as
//!   one query and returns `Vec<Scored<Ticket>>` — without `dorm-core` changing.
//! * Dynamic: [`semantic_search`] resolves everything through the registry, for
//!   the REPL and MCP.
//!
//! **Pre-filtering is mandatory and goes through the one compiler.** The query
//! set's own filters — the tenant included — are compiled by `compile_select_on`
//! into a subquery of primary keys, and the vector search ranks only those keys.
//! No filter SQL is written by hand, and the query vector is a bound parameter
//! with a cast (`$n::text::vector`) — the `Value` enum gains no variant. Because
//! the subquery's parameters are bound **first**, its `$1..$m` keep their numbers
//! and the search's own parameters follow as `$m+1..`, so no renumbering is
//! needed.

use std::collections::HashMap;
use std::marker::PhantomData;

use dorm_core::compiler::{self, Plan, Sql};
use dorm_core::db;
use dorm_core::error::{Error, Result};
use dorm_core::expr::Q;
use dorm_core::model::Model;
use dorm_core::queryset::QuerySet;
use dorm_core::registry;
use dorm_core::value::Value;

use crate::embedder::{default_embedder, Embedder};
use crate::migrations::shadow_table;
use crate::rags;
use crate::worker::{pk_value_from_text, vector_literal};

/// The RRF constant: a rank's contribution is `1/(k + rank)`. 60 is the value
/// from the original reciprocal-rank-fusion paper and the common default.
const RRF_K: i64 = 60;

/// A retrieved record with its relevance and the best-matching chunk.
#[derive(Debug, Clone)]
pub struct Scored<T> {
    pub item: T,
    pub score: f64,
    /// The chunk(s) that matched — today the single best chunk per record.
    pub chunks: Vec<String>,
}

/// The model-erased hit the dynamic path returns.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredRow {
    pub model: String,
    /// The primary key, as text (the uniform form the shadow table and outbox use).
    pub pk: String,
    pub score: f64,
    pub chunk: String,
}

/// Knobs shared by both front ends.
#[derive(Debug, Clone)]
pub struct SearchOptions {
    /// `Some(w)` turns on hybrid search: `w` weights the vector score, `1-w` the
    /// full-text score, fused by RRF. `None` is pure vector search.
    pub weight: Option<f64>,
    /// Drop hits scoring below this (cosine similarity for vector; the fused RRF
    /// score for hybrid).
    pub min_score: Option<f64>,
    pub limit: i64,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            weight: None,
            min_score: None,
            limit: 10,
        }
    }
}

/// One raw hit from the search SQL, before typed hydration.
struct Hit {
    pk: String,
    score: f64,
    chunk: String,
}

/// Build the search SQL and its parameters, pure, so it can be snapshot-tested.
///
/// `prefilter` is the compiled pk subquery; its parameters come first in the
/// returned vector, then the query vector, then (for hybrid) the query text and
/// weight, then the optional min-score and the limit.
fn build_search_sql(
    shadow: &str,
    prefilter: &Sql,
    embedding_literal: String,
    query_text: &str,
    opts: &SearchOptions,
) -> (String, Vec<Value>) {
    let mut params = prefilter.params.clone();
    let m = params.len();
    // The query vector.
    params.push(Value::Text(embedding_literal));
    let emb = m + 1;

    let (core, min_col) = match opts.weight {
        // ---- Hybrid: weighted reciprocal-rank fusion of vector + full-text. ----
        Some(weight) => {
            params.push(Value::Text(query_text.to_string()));
            let txt = m + 2;
            params.push(Value::F64(weight));
            let w = m + 3;
            let fused = format!(
                "(${w}::float8 * (1.0/({RRF_K} + vr)) + \
                  (1 - ${w}::float8) * (1.0/({RRF_K} + tr)))"
            );
            let sql = format!(
                "WITH cand AS (\n  \
                   SELECT r.record_pk, r.content, \
                          r.embedding <=> ${emb}::text::vector AS vdist, \
                          ts_rank(to_tsvector('simple', r.content), \
                                  plainto_tsquery('simple', ${txt})) AS trank \
                   FROM {shadow} r WHERE r.record_pk IN ({prefilter})\n), \
                 ranked AS (\n  \
                   SELECT record_pk, content, \
                          rank() OVER (ORDER BY vdist ASC) AS vr, \
                          rank() OVER (ORDER BY trank DESC) AS tr FROM cand\n), \
                 best AS (\n  \
                   SELECT DISTINCT ON (record_pk) record_pk::text AS pk, content, \
                          {fused} AS score \
                   FROM ranked ORDER BY record_pk, {fused} DESC\n)",
                prefilter = prefilter.text,
            );
            (sql, "best.score")
        }
        // ---- Pure vector: cosine similarity, best chunk per record. ----
        None => {
            let sql = format!(
                "WITH best AS (\n  \
                   SELECT DISTINCT ON (r.record_pk) r.record_pk::text AS pk, r.content, \
                          1 - (r.embedding <=> ${emb}::text::vector) AS score \
                   FROM {shadow} r WHERE r.record_pk IN ({prefilter}) \
                   ORDER BY r.record_pk, r.embedding <=> ${emb}::text::vector\n)",
                prefilter = prefilter.text,
            );
            (sql, "best.score")
        }
    };

    let mut tail = "SELECT pk, score, content FROM best".to_string();
    if let Some(min) = opts.min_score {
        params.push(Value::F64(min));
        let min_idx = params.len();
        tail.push_str(&format!(" WHERE {min_col} >= ${min_idx}::float8"));
    }
    params.push(Value::I64(opts.limit));
    let lim = params.len();
    tail.push_str(&format!(" ORDER BY score DESC LIMIT ${lim}"));

    (format!("{core}\n{tail}"), params)
}

/// Compile the pre-filter: the plan's filters (tenant included), as a subquery
/// selecting only the primary key — no order, no limit, no joins beyond those the
/// filters need.
fn prefilter_sql(plan: &Plan) -> Result<Sql> {
    let pk = plan.model.primary_key().ok_or_else(|| {
        Error::Config(format!(
            "model `{}` has no primary key, so it cannot be RAG-searched",
            plan.model.name
        ))
    })?;
    let mut pf = plan.clone();
    pf.values = vec![pk.name.to_string()];
    pf.order = Vec::new();
    pf.unordered = true;
    pf.limit = None;
    pf.offset = None;
    pf.distinct = false;
    pf.annotations = Vec::new();
    pf.select_related = Vec::new();
    compiler::compile_select_on(&pf, db::dialect())
}

/// Run a search described by `plan`'s filters and the already-computed query
/// `embedding`. The shared core of both front ends.
async fn execute(
    plan: &Plan,
    query_text: &str,
    embedding: &[f32],
    opts: &SearchOptions,
) -> Result<Vec<Hit>> {
    let meta = plan.model;
    // The model must actually have a RAG configuration.
    if !rags().any(|r| r.model == meta.name) {
        return Err(Error::Config(format!(
            "model `{}` has no `#[rag]`/`@@rag` configuration to search",
            meta.name
        )));
    }
    let shadow = shadow_table(meta.table);
    let prefilter = prefilter_sql(plan)?;
    let (sql, params) = build_search_sql(
        &shadow,
        &prefilter,
        vector_literal(embedding),
        query_text,
        opts,
    );
    let rows = db::fetch(&sql, &params).await?;
    rows.iter()
        .map(|r| {
            Ok(Hit {
                pk: r.try_get::<String>(0)?,
                score: r.try_get::<f64>(1)?,
                chunk: r.try_get::<String>(2)?,
            })
        })
        .collect()
}

/// Embed `query` with the installed default embedder, or explain how to set one.
async fn embed_default(query: &str) -> Result<Vec<f32>> {
    let e = default_embedder().ok_or_else(|| {
        Error::Config(
            "no default embedder installed — call `dorm_rag::install_embedder(..)` \
             at startup, or use `.search_with(&embedder)`"
                .to_string(),
        )
    })?;
    let mut v = e.embed_boxed(&[query.to_string()]).await?;
    v.pop()
        .ok_or_else(|| Error::Db("the embedder returned no vector for the query".into()))
}

/// The dynamic entry point: search `model` for `query`, pre-filtered by `filters`
/// (`filter_str` keys), with the installed default embedder.
pub async fn semantic_search(
    model: &str,
    query: &str,
    filters: &[(String, Value)],
    opts: &SearchOptions,
) -> Result<Vec<ScoredRow>> {
    let meta = registry::model(model)?;
    let mut plan = Plan::new(meta);
    for (k, v) in filters {
        plan.filters.push(Q::new(k.clone(), v.clone()));
    }
    let embedding = embed_default(query).await?;
    let hits = execute(&plan, query, &embedding, opts).await?;
    Ok(hits
        .into_iter()
        .map(|h| ScoredRow {
            model: model.to_string(),
            pk: h.pk,
            score: h.score,
            chunk: h.chunk,
        })
        .collect())
}

/// Extends `QuerySet<T>` with semantic search — the typed front end.
pub trait RagQuerySetExt<T: Model> {
    /// Begin a semantic search over this query set's filters.
    fn semantic(self, query: &str) -> RagQuery<T>;
}

impl<T: Model> RagQuerySetExt<T> for QuerySet<T> {
    fn semantic(self, query: &str) -> RagQuery<T> {
        RagQuery {
            plan: self.plan().clone(),
            query: query.to_string(),
            opts: SearchOptions::default(),
            _marker: PhantomData,
        }
    }
}

/// A semantic search in progress, built from a `QuerySet`'s filters.
pub struct RagQuery<T> {
    plan: Plan,
    query: String,
    opts: SearchOptions,
    _marker: PhantomData<T>,
}

impl<T: Model> RagQuery<T> {
    /// Turn on hybrid search: `weight` on the vector score, the rest on full-text.
    pub fn hybrid(mut self, weight: f64) -> Self {
        self.opts.weight = Some(weight);
        self
    }

    /// Drop hits below this score.
    pub fn min_score(mut self, score: f64) -> Self {
        self.opts.min_score = Some(score);
        self
    }

    /// Cap the number of records returned (default 10).
    pub fn limit(mut self, n: i64) -> Self {
        self.opts.limit = n;
        self
    }

    /// Run with an explicit embedder.
    pub async fn search_with<E: Embedder>(self, embedder: &E) -> Result<Vec<Scored<T>>> {
        let mut v = embedder.embed(std::slice::from_ref(&self.query)).await?;
        let embedding = v
            .pop()
            .ok_or_else(|| Error::Db("the embedder returned no vector for the query".into()))?;
        self.hydrate(&embedding).await
    }

    /// Run with the installed default embedder (see [`crate::install_embedder`]).
    pub async fn search(self) -> Result<Vec<Scored<T>>> {
        let embedding = embed_default(&self.query).await?;
        self.hydrate(&embedding).await
    }

    async fn hydrate(self, embedding: &[f32]) -> Result<Vec<Scored<T>>> {
        let pk = self.plan.model.primary_key().ok_or_else(|| {
            Error::Config(format!(
                "model `{}` has no primary key",
                self.plan.model.name
            ))
        })?;
        let hits = execute(&self.plan, &self.query, embedding, &self.opts).await?;
        if hits.is_empty() {
            return Ok(Vec::new());
        }

        // Hydrate the records in one query, then return them in score order.
        let pks: Vec<Value> = hits
            .iter()
            .map(|h| pk_value_from_text(&h.pk, pk.ty, pk.column))
            .collect::<Result<_>>()?;
        let items = T::objects()
            .filter(Q::many(format!("{}__in", pk.name), pks))
            .all()
            .await?;
        let mut by_pk: HashMap<String, T> = items
            .into_iter()
            .map(|it| (it.pk().to_string(), it))
            .collect();

        let mut out = Vec::with_capacity(hits.len());
        for h in hits {
            if let Some(item) = by_pk.remove(&h.pk) {
                out.push(Scored {
                    item,
                    score: h.score,
                    chunks: vec![h.chunk],
                });
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dorm_core::compiler::StatementKind;

    fn prefilter() -> Sql {
        Sql {
            text:
                "SELECT tickets.id FROM tickets WHERE tickets.status = $1 AND tickets.tenant = $2"
                    .to_string(),
            params: vec![Value::Text("open".into()), Value::Text("acme".into())],
            columns: Vec::new(),
            kind: StatementKind::Read,
        }
    }

    #[test]
    fn vector_search_binds_the_prefilter_params_first_then_the_vector_then_limit() {
        let opts = SearchOptions {
            weight: None,
            min_score: Some(0.35),
            limit: 10,
        };
        let (sql, params) = build_search_sql(
            "dorm_rag_tickets",
            &prefilter(),
            "[0.1,0.2]".into(),
            "q",
            &opts,
        );
        // The prefilter subquery is spliced in verbatim, keeping $1/$2.
        assert!(sql.contains("r.record_pk IN (SELECT tickets.id FROM tickets WHERE tickets.status = $1 AND tickets.tenant = $2)"), "{sql}");
        // The vector is a bound, cast parameter — never a literal in the text.
        assert!(sql.contains("$3::text::vector"), "{sql}");
        assert!(
            !sql.contains("[0.1,0.2]"),
            "the vector must not be spliced: {sql}"
        );
        // min_score then limit follow.
        assert!(sql.contains("best.score >= $4::float8"), "{sql}");
        assert!(sql.contains("LIMIT $5"), "{sql}");
        // Parameters, in order: prefilter, vector, min, limit.
        assert_eq!(
            params,
            vec![
                Value::Text("open".into()),
                Value::Text("acme".into()),
                Value::Text("[0.1,0.2]".into()),
                Value::F64(0.35),
                Value::I64(10),
            ]
        );
    }

    #[test]
    fn hybrid_search_adds_the_text_and_weight_params_and_fuses_by_rrf() {
        let opts = SearchOptions {
            weight: Some(0.7),
            min_score: None,
            limit: 5,
        };
        let (sql, params) = build_search_sql(
            "dorm_rag_tickets",
            &prefilter(),
            "[0.1]".into(),
            "email down",
            &opts,
        );
        assert!(sql.contains("plainto_tsquery('simple', $4)"), "{sql}");
        assert!(sql.contains("$5::float8"), "weight param: {sql}");
        assert!(sql.contains("1.0/(60 + vr)"), "rrf vector term: {sql}");
        assert!(sql.contains("1.0/(60 + tr)"), "rrf text term: {sql}");
        // prefilter($1,$2), vector($3), text($4), weight($5), limit($6) — no min.
        assert_eq!(
            params,
            vec![
                Value::Text("open".into()),
                Value::Text("acme".into()),
                Value::Text("[0.1]".into()),
                Value::Text("email down".into()),
                Value::F64(0.7),
                Value::I64(5),
            ]
        );
        assert!(sql.contains("LIMIT $6"), "{sql}");
    }
}
