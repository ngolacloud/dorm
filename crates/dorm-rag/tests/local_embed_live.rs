//! Live proof of the on-host ONNX embedder (ADR 0019, brick 4). It downloads the
//! model from Hugging Face on first run (then caches it) and runs entirely on the
//! host — no server. Skips if the model cannot be loaded (e.g. no network on the
//! first run).
//!
//! ```bash
//! cargo test -p dorm-rag --features embed-local --test local_embed_live -- --nocapture
//! ```

#![cfg(feature = "embed-local")]

use dorm_rag::embedder::{Embedder, LocalEmbedder};

#[tokio::test(flavor = "multi_thread")]
async fn the_local_embedder_runs_on_host_and_is_deterministic() {
    // Loading may download the model; do it off the async executor.
    let embedder = match tokio::task::spawn_blocking(LocalEmbedder::from_env)
        .await
        .expect("join")
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("skipped: could not load the local model ({e})");
            return;
        }
    };

    assert_eq!(embedder.model_id(), "all-MiniLM-L6-v2");
    assert_eq!(embedder.dimension(), 384, "all-MiniLM-L6-v2 is 384-dim");

    let vectors = embedder
        .embed(&[
            "cliente sem acesso ao email".to_string(),
            "fatura em dobro".to_string(),
        ])
        .await
        .expect("embed batch");
    assert_eq!(vectors.len(), 2, "one vector per input");
    assert_eq!(vectors[0].len(), 384);

    // Deterministic for the same input.
    let again = embedder
        .embed(&["cliente sem acesso ao email".to_string()])
        .await
        .expect("embed again");
    assert_eq!(again[0], vectors[0], "same text → same vector");

    // Related texts are closer than unrelated ones (a sanity check on meaning).
    let q = embedder
        .embed(&["não consigo entrar no webmail".to_string()])
        .await
        .expect("embed query")[0]
        .clone();
    let cos = |a: &[f32], b: &[f32]| -> f32 {
        let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        dot / (na * nb)
    };
    let sim_email = cos(&q, &vectors[0]); // vs "sem acesso ao email"
    let sim_bill = cos(&q, &vectors[1]); // vs "fatura em dobro"
    assert!(
        sim_email > sim_bill,
        "the email query is closer to the email text ({sim_email:.3}) than to billing ({sim_bill:.3})"
    );
}
