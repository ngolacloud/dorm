//! `RagMeta` — the RAG description, the way `ModelMeta` is the model description.
//!
//! Everything here is `&'static`, emitted by `#[derive(Rag)]` and submitted into
//! a distributed slice this crate owns. The shape is the point: it is the RAG
//! analogue of [`dorm_core::meta::ModelMeta`], read by the worker, the search
//! layer and `dorm rag status`, and tied to its model by **name** — never by a
//! type parameter — so a `RagMeta` for a model defined in another crate still
//! resolves through the registry, and `ModelMeta` itself never changes (ADR 0019).

/// The distance metric the vector index and the ranking use.
///
/// Mirrored to pgvector's operator classes and operators when the migrator and
/// the search SQL land (bricks 2 and 5); kept here as a closed set so a typo in
/// `#[rag(distance = "cosone")]` is a compile error, not a wrong index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceMetric {
    /// Cosine distance — the default, right for normalised embeddings.
    Cosine,
    /// L2 / Euclidean distance.
    L2,
    /// Negative inner product.
    InnerProduct,
}

impl DistanceMetric {
    /// Parse the `distance = "…"` attribute value. Returns `None` for an unknown
    /// spelling so the caller (the derive, or `dorm validate`) can name it.
    pub fn parse(s: &str) -> Option<DistanceMetric> {
        Some(match s {
            "cosine" => DistanceMetric::Cosine,
            "l2" | "euclidean" => DistanceMetric::L2,
            "inner_product" | "ip" | "dot" => DistanceMetric::InnerProduct,
            _ => return None,
        })
    }

    /// The pgvector operator-class suffix (`vector_cosine_ops`, …), for the HNSW
    /// index the migrator emits in brick 2.
    pub fn ops(self) -> &'static str {
        match self {
            DistanceMetric::Cosine => "vector_cosine_ops",
            DistanceMetric::L2 => "vector_l2_ops",
            DistanceMetric::InnerProduct => "vector_ip_ops",
        }
    }
}

/// One model's RAG configuration.
///
/// Emitted by `#[derive(Rag)]`, one per RAG model, and gathered by
/// [`inventory`](crate::inventory). The `model` field is the registered model
/// name (`"Ticket"`), which is how this joins its [`ModelMeta`](dorm_core::meta::ModelMeta)
/// without holding its type.
#[derive(Debug, Clone, Copy)]
pub struct RagMeta {
    /// The registered model name this configuration indexes, e.g. `"Ticket"`.
    pub model: &'static str,
    /// The source fields whose text is embedded. Each must be a `Text` column of
    /// the model (checked by [`crate::validate`] / `dorm validate`).
    pub fields: &'static [&'static str],
    /// The document template, with `{field}` placeholders drawn from `fields`.
    /// Rendered by the worker (brick 3) before chunking.
    pub template: &'static str,
    /// The embedder id, resolved against the configured embedders (brick 4).
    pub embedder: &'static str,
    /// Target chunk size, in characters, before overlap.
    pub chunk_size: u32,
    /// Characters of overlap between adjacent chunks.
    pub chunk_overlap: u32,
    /// Model columns copied into the shadow table as filterable metadata.
    pub metadata: &'static [&'static str],
    /// The distance metric for the index and the ranking.
    pub distance: DistanceMetric,
}

// The distributed slice, owned by dorm-rag — the RAG analogue of
// `inventory::collect!(ModelMeta)` in dorm-core. Each `#[derive(Rag)]` emits one
// `inventory::submit!`, and the linker gathers them; nothing enumerates RAG
// models by hand.
inventory::collect!(RagMeta);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_round_trips_through_its_attribute_spelling_and_maps_to_pgvector() {
        assert_eq!(
            DistanceMetric::parse("cosine"),
            Some(DistanceMetric::Cosine)
        );
        assert_eq!(DistanceMetric::parse("euclidean"), Some(DistanceMetric::L2));
        assert_eq!(
            DistanceMetric::parse("dot"),
            Some(DistanceMetric::InnerProduct)
        );
        assert_eq!(DistanceMetric::parse("cosone"), None);
        assert_eq!(DistanceMetric::Cosine.ops(), "vector_cosine_ops");
        assert_eq!(DistanceMetric::L2.ops(), "vector_l2_ops");
        assert_eq!(DistanceMetric::InnerProduct.ops(), "vector_ip_ops");
    }
}
