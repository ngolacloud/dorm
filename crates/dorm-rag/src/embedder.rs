//! The `Embedder` trait and its backends (ADR 0019, brick 4).
//!
//! An embedder turns text into vectors, in batches, and reports the dimension and
//! model id the shadow table and the staleness check need. The worker is generic
//! over this trait, so the choice of backend is the embedding project's.
//!
//! Backends (feature-gated, so named here as plain types rather than doc links):
//! `LocalEmbedder` (`embed-local`, ONNX on the host via `fastembed` — the
//! privacy-first default, data never leaves the box), `OllamaEmbedder`
//! (`embed-ollama`), `OpenAIEmbedder` for any OpenAI-compatible `/v1/embeddings`
//! endpoint (`embed-openai-compat`), and a deterministic [`HashEmbedder`] for
//! offline tests.

use std::future::Future;
use std::pin::Pin;
use std::sync::OnceLock;

use dorm_core::error::Result;
// `Error` is only constructed by the HTTP backends; gate it so a build without
// any of them carries no unused import.
#[cfg(any(
    feature = "embed-ollama",
    feature = "embed-openai-compat",
    feature = "embed-local"
))]
use dorm_core::error::Error;

/// Turn text into vectors. Batched, async, and `Send` so the worker can await it
/// across a multithreaded runtime.
///
/// `embed` is spelled as `-> impl Future + Send` rather than `async fn` on
/// purpose: it keeps the `Send` bound explicit (the worker needs it) and avoids
/// the `async_fn_in_trait` lint on a public trait.
pub trait Embedder {
    /// The embedder/model id recorded in the shadow table, e.g. `"all-minilm"`.
    /// A change here marks existing chunks stale (the worker re-embeds them).
    fn model_id(&self) -> &str;

    /// The vector dimension, which must match the shadow table's `vector(dim)`.
    fn dimension(&self) -> u32;

    /// Embed a batch of texts, returning one vector per input, in order.
    fn embed(&self, texts: &[String]) -> impl Future<Output = Result<Vec<Vec<f32>>>> + Send;
}

/// The object-safe face of [`Embedder`], so a `dyn` embedder can be installed as
/// the process default and used by the no-argument `QuerySet::semantic(..).search()`
/// and the dynamic `semantic_search`. `Embedder`'s `-> impl Future` is not
/// object-safe; this boxes the future instead. A blanket impl covers every
/// `Embedder`, so callers only ever write the simpler trait.
/// The boxed future [`DynEmbedder::embed_boxed`] returns.
pub type EmbedFuture<'a> = Pin<Box<dyn Future<Output = Result<Vec<Vec<f32>>>> + Send + 'a>>;

pub trait DynEmbedder: Send + Sync {
    fn model_id(&self) -> &str;
    fn dimension(&self) -> u32;
    fn embed_boxed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a>;
}

impl<E: Embedder + Send + Sync> DynEmbedder for E {
    fn model_id(&self) -> &str {
        Embedder::model_id(self)
    }
    fn dimension(&self) -> u32 {
        Embedder::dimension(self)
    }
    fn embed_boxed<'a>(&'a self, texts: &'a [String]) -> EmbedFuture<'a> {
        Box::pin(self.embed(texts))
    }
}

static DEFAULT_EMBEDDER: OnceLock<Box<dyn DynEmbedder>> = OnceLock::new();

/// Install the process-default embedder, used by `search()`/`semantic_search`
/// when no embedder is passed explicitly. Idempotent after the first call (a
/// second call is ignored), so a binary sets it once at startup.
pub fn install_embedder<E: Embedder + Send + Sync + 'static>(embedder: E) {
    let _ = DEFAULT_EMBEDDER.set(Box::new(embedder));
}

/// The installed default embedder, if any.
pub fn default_embedder() -> Option<&'static dyn DynEmbedder> {
    DEFAULT_EMBEDDER.get().map(|b| b.as_ref())
}

/// A deterministic, dependency-free embedder for tests: a bag-of-bytes hash
/// projected onto `dim` dimensions and L2-normalised. Not meaningful semantics —
/// but stable, offline, and enough to prove the worker's plumbing (chunking,
/// upsert, staleness, delete) without a model server.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: u32,
}

impl HashEmbedder {
    pub fn new(dim: u32) -> Self {
        Self { dim }
    }
}

impl Embedder for HashEmbedder {
    fn model_id(&self) -> &str {
        "hash-test"
    }

    fn dimension(&self) -> u32 {
        self.dim
    }

    fn embed(&self, texts: &[String]) -> impl Future<Output = Result<Vec<Vec<f32>>>> + Send {
        let dim = self.dim as usize;
        let texts = texts.to_vec();
        async move {
            Ok(texts
                .iter()
                .map(|t| {
                    let mut v = vec![0f32; dim];
                    for (i, b) in t.bytes().enumerate() {
                        v[i % dim] += f32::from(b);
                    }
                    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if norm > 0.0 {
                        for x in &mut v {
                            *x /= norm;
                        }
                    }
                    v
                })
                .collect())
        }
    }
}

/// Embed through a local or remote [Ollama](https://ollama.com) server
/// (feature `embed-ollama`), using its `/api/embed` batch endpoint. The default
/// model is `all-minilm` (384-dim) — the smallest practical embedding model — so
/// data never leaves the host unless the URL points elsewhere.
///
/// Configuration (read once, at construction), redaction-safe — no secret is ever
/// logged:
/// * `DELONIX_ORM_RAG_OLLAMA_URL` — base URL (default `http://localhost:11434`);
/// * `DELONIX_ORM_RAG_EMBEDDER` — model name (default `all-minilm`).
#[cfg(feature = "embed-ollama")]
#[derive(Debug, Clone)]
pub struct OllamaEmbedder {
    base_url: String,
    model: String,
    dim: u32,
    client: reqwest::Client,
}

#[cfg(feature = "embed-ollama")]
impl OllamaEmbedder {
    /// Build from the `DELONIX_ORM_RAG_*` environment, probing the model once for
    /// its dimension so it matches the shadow table.
    pub async fn from_env() -> Result<Self> {
        let base_url = std::env::var("DELONIX_ORM_RAG_OLLAMA_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        let model =
            std::env::var("DELONIX_ORM_RAG_EMBEDDER").unwrap_or_else(|_| "all-minilm".to_string());
        Self::connect(&base_url, &model).await
    }

    /// Build for an explicit URL and model, probing the dimension.
    pub async fn connect(base_url: &str, model: &str) -> Result<Self> {
        let client = reqwest::Client::new();
        let mut e = Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            dim: 0,
            client,
        };
        let probe = e.embed(&["dimension probe".to_string()]).await?;
        e.dim = probe
            .first()
            .map(|v| v.len() as u32)
            .filter(|d| *d > 0)
            .ok_or_else(|| {
                Error::Config(format!(
                    "Ollama model `{model}` returned an empty embedding"
                ))
            })?;
        Ok(e)
    }
}

#[cfg(feature = "embed-ollama")]
impl Embedder for OllamaEmbedder {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimension(&self) -> u32 {
        self.dim
    }

    fn embed(&self, texts: &[String]) -> impl Future<Output = Result<Vec<Vec<f32>>>> + Send {
        let url = format!("{}/api/embed", self.base_url);
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let client = self.client.clone();
        async move {
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| Error::Db(format!("Ollama request failed: {e}")))?;
            if !resp.status().is_success() {
                return Err(Error::Db(format!("Ollama returned HTTP {}", resp.status())));
            }
            #[derive(serde::Deserialize)]
            struct EmbedResponse {
                embeddings: Vec<Vec<f32>>,
            }
            let parsed: EmbedResponse = resp.json().await.map_err(|e| {
                Error::Db(format!("Ollama response was not the expected JSON: {e}"))
            })?;
            Ok(parsed.embeddings)
        }
    }
}

/// Embed through any **OpenAI-compatible** `/v1/embeddings` endpoint (feature
/// `embed-openai-compat`) — OpenAI itself, Azure OpenAI, a local vLLM/LM Studio,
/// or Ollama's own `/v1` shim. This is the "bring your own hosted embedder" path.
///
/// Configuration (read once, at construction):
/// * `DELONIX_ORM_RAG_OPENAI_URL` — base URL incl. `/v1` (default
///   `https://api.openai.com/v1`);
/// * `DELONIX_ORM_RAG_OPENAI_KEY` — the API key, sent as a bearer and **never
///   logged** (optional: a local server may need none);
/// * `DELONIX_ORM_RAG_EMBEDDER` — the model (default `text-embedding-3-small`).
#[cfg(feature = "embed-openai-compat")]
#[derive(Clone)]
pub struct OpenAIEmbedder {
    base_url: String,
    model: String,
    api_key: Option<String>,
    dim: u32,
    client: reqwest::Client,
}

// A hand-written Debug so the API key can never leak into a log or panic.
#[cfg(feature = "embed-openai-compat")]
impl std::fmt::Debug for OpenAIEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAIEmbedder")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "***"))
            .field("dim", &self.dim)
            .finish()
    }
}

#[cfg(feature = "embed-openai-compat")]
impl OpenAIEmbedder {
    /// Build from the `DELONIX_ORM_RAG_*` environment, probing the dimension.
    pub async fn from_env() -> Result<Self> {
        let base_url = std::env::var("DELONIX_ORM_RAG_OPENAI_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let model = std::env::var("DELONIX_ORM_RAG_EMBEDDER")
            .unwrap_or_else(|_| "text-embedding-3-small".to_string());
        let api_key = std::env::var("DELONIX_ORM_RAG_OPENAI_KEY").ok();
        Self::connect(&base_url, &model, api_key).await
    }

    /// Build for an explicit URL, model and optional key, probing the dimension.
    pub async fn connect(base_url: &str, model: &str, api_key: Option<String>) -> Result<Self> {
        let mut e = Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key,
            dim: 0,
            client: reqwest::Client::new(),
        };
        let probe = e.embed(&["dimension probe".to_string()]).await?;
        e.dim = probe
            .first()
            .map(|v| v.len() as u32)
            .filter(|d| *d > 0)
            .ok_or_else(|| Error::Config(format!("model `{model}` returned an empty embedding")))?;
        Ok(e)
    }
}

#[cfg(feature = "embed-openai-compat")]
impl Embedder for OpenAIEmbedder {
    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimension(&self) -> u32 {
        self.dim
    }

    fn embed(&self, texts: &[String]) -> impl Future<Output = Result<Vec<Vec<f32>>>> + Send {
        let url = format!("{}/embeddings", self.base_url);
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let client = self.client.clone();
        let key = self.api_key.clone();
        async move {
            let mut req = client.post(&url).json(&body);
            if let Some(k) = key {
                req = req.bearer_auth(k);
            }
            let resp = req
                .send()
                .await
                .map_err(|e| Error::Db(format!("embeddings request failed: {e}")))?;
            if !resp.status().is_success() {
                // Never echo the body — it can contain the request, hence the key.
                return Err(Error::Db(format!(
                    "embeddings endpoint returned HTTP {}",
                    resp.status()
                )));
            }
            #[derive(serde::Deserialize)]
            struct Item {
                embedding: Vec<f32>,
                #[serde(default)]
                index: usize,
            }
            #[derive(serde::Deserialize)]
            struct EmbedResponse {
                data: Vec<Item>,
            }
            let mut parsed: EmbedResponse = resp.json().await.map_err(|e| {
                Error::Db(format!(
                    "embeddings response was not the expected JSON: {e}"
                ))
            })?;
            // The spec returns an `index` per item; sort by it so order matches the
            // input regardless of how the server batched.
            parsed.data.sort_by_key(|i| i.index);
            Ok(parsed.data.into_iter().map(|i| i.embedding).collect())
        }
    }
}

/// Embed **on the host** with an ONNX model via `fastembed` (feature
/// `embed-local`) — no server, no network at query time, so the data never
/// leaves the box. This is the privacy-first default of ADR 0019. The model is
/// downloaded once (from Hugging Face) and cached; thereafter it runs locally.
///
/// `DELONIX_ORM_RAG_EMBEDDER` picks the model (default `all-minilm`, i.e.
/// `all-MiniLM-L6-v2`, 384-dim). Inference is synchronous, so [`embed`](Embedder::embed)
/// runs it on `spawn_blocking` to keep the async executor free.
#[cfg(feature = "embed-local")]
#[derive(Clone)]
pub struct LocalEmbedder {
    model: std::sync::Arc<std::sync::Mutex<fastembed::TextEmbedding>>,
    model_id: String,
    dim: u32,
}

#[cfg(feature = "embed-local")]
impl std::fmt::Debug for LocalEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalEmbedder")
            .field("model_id", &self.model_id)
            .field("dim", &self.dim)
            .finish()
    }
}

#[cfg(feature = "embed-local")]
impl LocalEmbedder {
    /// Build from the `DELONIX_ORM_RAG_*` environment, loading and probing the
    /// model. Blocking (it may download the model on first use), so call it off
    /// the async executor (e.g. before `serve`, or via `spawn_blocking`).
    pub fn from_env() -> Result<Self> {
        let name =
            std::env::var("DELONIX_ORM_RAG_EMBEDDER").unwrap_or_else(|_| "all-minilm".to_string());
        Self::load(&name)
    }

    /// Load a named model. Known aliases map onto `fastembed`'s model set; an
    /// unknown name is an explicit error listing what is supported.
    pub fn load(name: &str) -> Result<Self> {
        use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
        let (model, model_id) = match name {
            "all-minilm" | "all-MiniLM-L6-v2" | "AllMiniLML6V2" => {
                (EmbeddingModel::AllMiniLML6V2, "all-MiniLM-L6-v2")
            }
            "bge-small-en" | "BGESmallENV15" => {
                (EmbeddingModel::BGESmallENV15, "bge-small-en-v1.5")
            }
            other => {
                return Err(Error::Config(format!(
                    "unknown local model `{other}`; known: all-minilm, bge-small-en"
                )))
            }
        };
        let te = TextEmbedding::try_new(InitOptions::new(model).with_show_download_progress(false))
            .map_err(|e| Error::Config(format!("loading local embedder `{model_id}`: {e}")))?;
        // Probe the dimension with one embedding.
        let probe = te
            .embed(vec!["dimension probe"], None)
            .map_err(|e| Error::Db(format!("local embedder probe failed: {e}")))?;
        let dim = probe
            .first()
            .map(|v| v.len() as u32)
            .filter(|d| *d > 0)
            .ok_or_else(|| Error::Config("local embedder returned an empty vector".into()))?;
        Ok(Self {
            model: std::sync::Arc::new(std::sync::Mutex::new(te)),
            model_id: model_id.to_string(),
            dim,
        })
    }
}

#[cfg(feature = "embed-local")]
impl Embedder for LocalEmbedder {
    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimension(&self) -> u32 {
        self.dim
    }

    fn embed(&self, texts: &[String]) -> impl Future<Output = Result<Vec<Vec<f32>>>> + Send {
        let model = self.model.clone();
        let texts = texts.to_vec();
        async move {
            // ONNX inference is synchronous and CPU-bound: run it on a blocking
            // thread so it does not stall the async executor.
            tokio::task::spawn_blocking(move || {
                let guard = model
                    .lock()
                    .map_err(|_| Error::Db("embedder mutex poisoned".into()))?;
                guard
                    .embed(texts, None)
                    .map_err(|e| Error::Db(format!("local embed failed: {e}")))
            })
            .await
            .map_err(|e| Error::Db(format!("embedding task failed: {e}")))?
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_hash_embedder_is_deterministic_and_normalised() {
        let e = HashEmbedder::new(8);
        assert_eq!(Embedder::dimension(&e), 8);
        let a = e.embed(&["hello".to_string()]).await.unwrap();
        let b = e.embed(&["hello".to_string()]).await.unwrap();
        assert_eq!(a, b, "deterministic");
        assert_eq!(a[0].len(), 8);
        let norm: f32 = a[0].iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "unit length, got {norm}");
    }
}
