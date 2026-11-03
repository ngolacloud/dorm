//! Live proof of the OpenAI-compatible embedder (ADR 0019, brick 4). It runs
//! against any `/v1/embeddings` endpoint; the lab uses Ollama's own OpenAI shim,
//! so it needs no API key and no cloud. Skips if that endpoint is unreachable.
//!
//! ```bash
//! cargo test -p dorm-rag --features embed-openai-compat --test openai_embed_live -- --nocapture
//! ```

#![cfg(feature = "embed-openai-compat")]

use dorm_rag::embedder::{Embedder, OpenAIEmbedder};

#[tokio::test]
async fn the_openai_compatible_embedder_embeds_through_v1() {
    // Default to Ollama's OpenAI-compatible endpoint; no key needed there.
    let url = std::env::var("DELONIX_ORM_RAG_OPENAI_URL")
        .unwrap_or_else(|_| "http://localhost:11434/v1".to_string());
    let model =
        std::env::var("DELONIX_ORM_RAG_EMBEDDER").unwrap_or_else(|_| "all-minilm".to_string());
    let key = std::env::var("DELONIX_ORM_RAG_OPENAI_KEY").ok();

    let embedder = match OpenAIEmbedder::connect(&url, &model, key).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipped: {url} not reachable ({e})");
            return;
        }
    };

    assert_eq!(embedder.model_id(), model);
    assert!(embedder.dimension() > 0, "probed a dimension");

    // A batch of two, returned in input order (the impl sorts by `index`).
    let vectors = embedder
        .embed(&[
            "cliente sem acesso ao email".to_string(),
            "fatura em dobro".to_string(),
        ])
        .await
        .expect("embed batch");
    assert_eq!(vectors.len(), 2, "one vector per input");
    assert_eq!(vectors[0].len() as u32, embedder.dimension());
    assert_eq!(vectors[1].len() as u32, embedder.dimension());

    // Deterministic for the same input.
    let again = embedder
        .embed(&["cliente sem acesso ao email".to_string()])
        .await
        .expect("embed again");
    assert_eq!(again[0], vectors[0], "same text → same vector");
}
