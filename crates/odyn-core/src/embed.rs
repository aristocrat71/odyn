//! Text embeddings for memory retrieval, from any of three backends.
//!
//! - **builtin** — fastembed, bundled and local. Zero setup, works offline,
//!   downloads its weights once into the data dir.
//! - **ollama** — the local daemon's embedding models, over `/api/embed`.
//!   Also local and offline, and nothing has to be loaded into this process.
//! - **a configured provider** — an OpenAI-compatible `/embeddings` route.
//!   This one sends note text off the machine; callers are expected to say so
//!   out loud, because it is the only backend that does.
//!
//! Vectors from two different models are not comparable — not even at equal
//! width — so changing the model means re-embedding every note. `storage`
//! enforces that; this module only produces the vectors, and reports how wide
//! they are by [`probe_dim`] rather than by a hard-coded table.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

use crate::config::{Config, ProviderConfig};

const MODEL_CACHE_DIR_NAME: &str = "models";

/// The input window Odyn asks for. fastembed's own default is one constant for
/// every model, which would clip a long-context model down to a short model's
/// window; asking for the largest on offer and letting `load_tokenizer` clamp
/// it to each model's real `model_max_length` is what makes an 8k model
/// actually see 8k. Text past a model's window is truncated before embedding,
/// so it cannot influence whether that note is recalled.
const REQUESTED_INPUT_TOKENS: usize = 8192;

/// What [`probe_dim`] embeds to learn a model's width. Any short text does.
const PROBE_TEXT: &str = "odyn";

const HTTP_TIMEOUT: Duration = Duration::from_secs(120);

/// Short, friendly spellings for the models people actually reach for. These
/// are aliases, never a gate: any name fastembed knows is accepted too, so no
/// model is excluded by this table being short.
const ALIASES: &[(&str, EmbeddingModel)] = &[
    ("bge-small", EmbeddingModel::BGESmallENV15),
    ("bge-base", EmbeddingModel::BGEBaseENV15),
    ("bge-large", EmbeddingModel::BGELargeENV15),
    ("bge-m3", EmbeddingModel::BGEM3),
    ("mxbai-large", EmbeddingModel::MxbaiEmbedLargeV1),
    ("nomic-v1.5", EmbeddingModel::NomicEmbedTextV15),
    ("arctic-m-long", EmbeddingModel::SnowflakeArcticEmbedMLong),
    ("jina-code", EmbeddingModel::JinaEmbeddingsV2BaseCode),
    ("e5-small", EmbeddingModel::MultilingualE5Small),
    ("e5-base", EmbeddingModel::MultilingualE5Base),
    ("e5-large", EmbeddingModel::MultilingualE5Large),
    ("minilm-l6", EmbeddingModel::AllMiniLML6V2),
    ("gte-base", EmbeddingModel::GTEBaseENV15),
    ("gte-large", EmbeddingModel::GTELargeENV15),
];

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("could not load the embedding model: {0}")]
    Load(String),
    #[error("embedding failed: {0}")]
    Embed(String),
    #[error("could not locate a data directory for the embedding model cache")]
    NoDataDir,
    #[error("`{0}` is not a model fastembed knows; pick one from the brain view")]
    UnknownBuiltin(String),
    #[error("no provider named `{0}` is configured, so `{0}:` names no endpoint")]
    UnknownProvider(String),
    #[error("{backend} did not answer: {message}")]
    Unreachable { backend: String, message: String },
}

/// Where an embedding model runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedBackend {
    /// fastembed, in this process.
    Builtin,
    /// The local Ollama daemon.
    Ollama,
    /// A configured OpenAI-compatible provider, by name. The only backend
    /// that sends note text off this machine.
    Provider(String),
}

/// Which model embeds this brain: a backend and a name within it. Written in
/// `odyn.toml` as `bge-small`, `ollama:nomic-embed-text` or `zen:some-model`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedModel {
    pub backend: EmbedBackend,
    pub name: String,
}

impl Default for EmbedModel {
    fn default() -> Self {
        Self {
            backend: EmbedBackend::Builtin,
            name: builtin_name(EmbeddingModel::BGESmallENV15),
        }
    }
}

impl EmbedModel {
    /// Never fails: an unresolvable name is a brain that cannot embed, which
    /// is reported when something tries to — not a config file that refuses
    /// to load and takes chat down with it.
    pub fn parse(text: &str) -> Self {
        let text = text.trim();
        match text.split_once(':') {
            // Ollama tags carry colons of their own (`model:latest`), so only
            // the first one separates the backend.
            Some(("ollama", name)) => Self {
                backend: EmbedBackend::Ollama,
                name: name.to_string(),
            },
            Some((provider, name)) if !provider.is_empty() && !name.is_empty() => Self {
                backend: EmbedBackend::Provider(provider.to_string()),
                name: name.to_string(),
            },
            _ => Self {
                backend: EmbedBackend::Builtin,
                name: text.to_string(),
            },
        }
    }

    /// The exact string `odyn.toml` holds and `brain_meta` records. Comparing
    /// these is what decides whether the index must be rebuilt.
    pub fn canonical(&self) -> String {
        match &self.backend {
            EmbedBackend::Builtin => self.name.clone(),
            EmbedBackend::Ollama => format!("ollama:{}", self.name),
            EmbedBackend::Provider(provider) => format!("{provider}:{}", self.name),
        }
    }

    /// Whether using this model sends note text off the machine.
    pub fn is_remote(&self) -> bool {
        matches!(self.backend, EmbedBackend::Provider(_))
    }

    /// The width, when it is knowable without asking the model — fastembed
    /// publishes it, an HTTP endpoint does not.
    pub fn known_dim(&self) -> Option<usize> {
        match self.backend {
            EmbedBackend::Builtin => {
                let wanted = builtin_model(&self.name)?;
                catalog_entry(wanted).map(|info| info.dim)
            }
            _ => None,
        }
    }
}

impl fmt::Display for EmbedModel {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(&self.canonical())
    }
}

impl FromStr for EmbedModel {
    type Err = std::convert::Infallible;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(text))
    }
}

impl serde::Serialize for EmbedModel {
    fn serialize<S: serde::Serializer>(&self, out: S) -> Result<S::Ok, S::Error> {
        out.serialize_str(&self.canonical())
    }
}

impl<'de> serde::Deserialize<'de> for EmbedModel {
    fn deserialize<D: serde::Deserializer<'de>>(input: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(input)?))
    }
}

/// One selectable model, for the picker.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EmbedOption {
    /// What goes in the config: `bge-small`, `ollama:nomic-embed-text`, …
    pub id: String,
    pub backend: &'static str,
    /// Known ahead of time only for builtin models.
    pub dim: Option<usize>,
    pub description: String,
    pub remote: bool,
}

/// Every fastembed model, by its friendly alias where one exists and by
/// fastembed's own canonical name otherwise — the whole catalog, not a
/// hand-picked slice of it.
pub fn builtin_catalog() -> Vec<EmbedOption> {
    let mut options: Vec<EmbedOption> = TextEmbedding::list_supported_models()
        .into_iter()
        .map(|info| EmbedOption {
            id: builtin_name(info.model),
            backend: "builtin",
            dim: Some(info.dim),
            description: info.description,
            remote: false,
        })
        .collect();
    options.sort_by(|a, b| a.id.cmp(&b.id));
    options
}

fn catalog_entry(model: EmbeddingModel) -> Option<fastembed::ModelInfo<EmbeddingModel>> {
    TextEmbedding::list_supported_models()
        .into_iter()
        .find(|info| info.model == model)
}

/// The alias if this model has one, else fastembed's own canonical spelling.
fn builtin_name(model: EmbeddingModel) -> String {
    ALIASES
        .iter()
        .find(|(_, known)| *known == model)
        .map(|(alias, _)| (*alias).to_string())
        .unwrap_or_else(|| model.to_string())
}

/// Aliases first, then anything fastembed itself accepts (case-insensitively).
fn builtin_model(name: &str) -> Option<EmbeddingModel> {
    ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map(|(_, model)| model.clone())
        .or_else(|| EmbeddingModel::from_str(name).ok())
}

pub trait Embedder {
    /// One vector per input text, in input order, each as wide as the model
    /// that produced it.
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// How wide this embedder's vectors are, learned by embedding one short text.
/// The only way to know for a backend that does not publish a catalog, and
/// cheap enough to be the uniform answer for all of them.
pub fn probe_dim(embedder: &mut dyn Embedder) -> Result<usize, EmbedError> {
    let vector = embedder
        .embed(&[PROBE_TEXT])?
        .into_iter()
        .next()
        .ok_or_else(|| EmbedError::Embed("the embedder returned no vector".to_string()))?;
    if vector.is_empty() {
        return Err(EmbedError::Embed(
            "the embedder returned an empty vector".to_string(),
        ));
    }
    Ok(vector.len())
}

/// Builds the embedder `model` names, reading endpoints and keys from
/// `config`. Nothing is downloaded or connected to until this is called, so
/// an unused model costs nothing.
pub fn load_embedder(config: &Config, model: &EmbedModel) -> Result<Box<dyn Embedder>, EmbedError> {
    match &model.backend {
        EmbedBackend::Builtin => {
            let named = builtin_model(&model.name)
                .ok_or_else(|| EmbedError::UnknownBuiltin(model.name.clone()))?;
            Ok(Box::new(FastEmbedder::load(
                named,
                &embedding_cache_dir()?,
            )?))
        }
        EmbedBackend::Ollama => {
            let base_url = ollama_base_url(config);
            Ok(Box::new(HttpEmbedder::ollama(&base_url, &model.name)?))
        }
        EmbedBackend::Provider(name) => {
            let provider = config
                .providers
                .get(name)
                .ok_or_else(|| EmbedError::UnknownProvider(name.clone()))?;
            let ProviderConfig::OpenAiCompat { base_url, .. } = provider else {
                return Err(EmbedError::UnknownProvider(name.clone()));
            };
            let key = provider
                .api_key(name)
                .map_err(|err| EmbedError::Load(err.to_string()))?;
            Ok(Box::new(HttpEmbedder::openai(base_url, &model.name, key)?))
        }
    }
}

/// The configured Ollama endpoint, or the default. `ollama:` means the local
/// daemon whichever entry in the file happens to describe it.
fn ollama_base_url(config: &Config) -> String {
    config
        .providers
        .values()
        .find_map(|provider| match provider {
            ProviderConfig::Ollama { base_url, .. } => Some(base_url.clone()),
            ProviderConfig::OpenAiCompat { .. } => None,
        })
        .unwrap_or_else(|| "http://localhost:11434".to_string())
}

pub struct FastEmbedder {
    model: TextEmbedding,
}

impl FastEmbedder {
    /// Loads the model, downloading it into `cache_dir` the first time.
    pub fn load(model: EmbeddingModel, cache_dir: &Path) -> Result<Self, EmbedError> {
        let options = TextInitOptions::new(model)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_max_length(REQUESTED_INPUT_TOKENS)
            .with_show_download_progress(true);
        let loaded =
            TextEmbedding::try_new(options).map_err(|error| EmbedError::Load(error.to_string()))?;
        Ok(Self { model: loaded })
    }
}

impl Embedder for FastEmbedder {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        self.model
            .embed(texts, None)
            .map_err(|error| EmbedError::Embed(error.to_string()))
    }
}

/// Ollama's `/api/embed` and the OpenAI-compatible `/embeddings` differ only
/// in the shape of one request and one response, so they share a client.
enum Wire {
    Ollama,
    OpenAi,
}

pub struct HttpEmbedder {
    client: reqwest::Client,
    url: String,
    model: String,
    api_key: Option<String>,
    wire: Wire,
    backend: String,
}

impl HttpEmbedder {
    fn ollama(base_url: &str, model: &str) -> Result<Self, EmbedError> {
        Ok(Self {
            client: client()?,
            url: format!("{}/api/embed", base_url.trim_end_matches('/')),
            model: model.to_string(),
            api_key: None,
            wire: Wire::Ollama,
            backend: "ollama".to_string(),
        })
    }

    fn openai(base_url: &str, model: &str, api_key: Option<String>) -> Result<Self, EmbedError> {
        Ok(Self {
            client: client()?,
            url: format!("{}/embeddings", base_url.trim_end_matches('/')),
            model: model.to_string(),
            api_key,
            wire: Wire::OpenAi,
            backend: base_url.to_string(),
        })
    }
}

fn client() -> Result<reqwest::Client, EmbedError> {
    reqwest::Client::builder()
        .timeout(HTTP_TIMEOUT)
        .build()
        .map_err(|err| EmbedError::Load(err.to_string()))
}

impl Embedder for HttpEmbedder {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        // Both wires take the same request; they differ in what comes back.
        let body = serde_json::json!({ "model": self.model, "input": texts });
        let mut request = self.client.post(&self.url).json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let backend = self.backend.clone();
        let payload: serde_json::Value = blocking(async move {
            let response = request
                .send()
                .await
                .map_err(|err| EmbedError::Unreachable {
                    backend: backend.clone(),
                    message: err.to_string(),
                })?;
            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|err| EmbedError::Unreachable {
                    backend: backend.clone(),
                    message: err.to_string(),
                })?;
            if !status.is_success() {
                return Err(EmbedError::Embed(format!(
                    "{backend} returned {status}: {}",
                    text.chars().take(300).collect::<String>()
                )));
            }
            serde_json::from_str(&text).map_err(|err| EmbedError::Embed(err.to_string()))
        })?;

        let vectors = match self.wire {
            Wire::Ollama => ollama_vectors(&payload),
            Wire::OpenAi => openai_vectors(&payload),
        }
        .ok_or_else(|| EmbedError::Embed(format!("{} sent no usable embeddings", self.backend)))?;
        if vectors.len() != texts.len() {
            return Err(EmbedError::Embed(format!(
                "asked for {} embeddings, got {}",
                texts.len(),
                vectors.len()
            )));
        }
        Ok(vectors)
    }
}

fn ollama_vectors(payload: &serde_json::Value) -> Option<Vec<Vec<f32>>> {
    payload
        .get("embeddings")?
        .as_array()?
        .iter()
        .map(numbers)
        .collect()
}

/// `data` carries its own `index`, and nothing promises it arrives in order.
fn openai_vectors(payload: &serde_json::Value) -> Option<Vec<Vec<f32>>> {
    let mut rows: Vec<(u64, Vec<f32>)> = payload
        .get("data")?
        .as_array()?
        .iter()
        .map(|row| {
            let index = row
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            numbers(row.get("embedding")?).map(|vector| (index, vector))
        })
        .collect::<Option<Vec<_>>>()?;
    rows.sort_by_key(|(index, _)| *index);
    Some(rows.into_iter().map(|(_, vector)| vector).collect())
}

fn numbers(value: &serde_json::Value) -> Option<Vec<f32>> {
    value
        .as_array()?
        .iter()
        .map(|number| number.as_f64().map(|number| number as f32))
        .collect()
}

/// Runs one request to completion from a synchronous caller. The whole brain
/// pipeline is sync — it runs inside `spawn_blocking` in the app and inside a
/// current-thread runtime in the CLI — and starting a runtime inside either
/// would panic, so the work goes to a scratch thread that has no runtime of
/// its own to collide with.
fn blocking<T, F>(future: F) -> Result<T, EmbedError>
where
    F: std::future::Future<Output = Result<T, EmbedError>> + Send,
    T: Send,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|err| EmbedError::Load(err.to_string()))?
                    .block_on(future)
            })
            .join()
            .map_err(|_| EmbedError::Embed("the embedding request panicked".to_string()))?
    })
}

/// Where model files live: the platform data dir, next to the database.
pub fn embedding_cache_dir() -> Result<PathBuf, EmbedError> {
    let dirs = directories::ProjectDirs::from("", "", "odyn").ok_or(EmbedError::NoDataDir)?;
    Ok(dirs.data_dir().join(MODEL_CACHE_DIR_NAME))
}

/// Deterministic stand-in for tests: an LCG seeded from the text's bytes,
/// L2-normalized like the real model's output.
#[cfg(test)]
pub(crate) struct FakeEmbedder;

#[cfg(test)]
pub(crate) const FAKE_DIM: usize = 384;

#[cfg(test)]
impl Embedder for FakeEmbedder {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|text| fake_embedding(text)).collect())
    }
}

#[cfg(test)]
pub(crate) fn fake_embedding(text: &str) -> Vec<f32> {
    let mut state = text.bytes().fold(0x9E37_79B9_7F4A_7C15u64, |state, byte| {
        state.wrapping_mul(31).wrapping_add(u64::from(byte))
    });
    let mut values: Vec<f32> = (0..FAKE_DIM)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as f32 / (1u64 << 31) as f32) - 1.0
        })
        .collect();
    let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
    for value in &mut values {
        *value /= norm;
    }
    values
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_embeddings_are_deterministic_unit_vectors() {
        let mut embedder = FakeEmbedder;
        let embeddings = embedder.embed(&["alpha", "alpha", "beta"]).expect("embed");
        assert_eq!(embeddings.len(), 3);
        for embedding in &embeddings {
            assert_eq!(embedding.len(), FAKE_DIM);
            let norm = embedding
                .iter()
                .map(|value| value * value)
                .sum::<f32>()
                .sqrt();
            assert!((norm - 1.0).abs() < 1e-5, "norm {norm}");
        }
        assert_eq!(embeddings[0], embeddings[1]);
        assert_ne!(embeddings[0], embeddings[2]);
    }

    #[test]
    fn probe_dim_reports_the_width_of_what_it_embedded() {
        assert_eq!(probe_dim(&mut FakeEmbedder).expect("probe"), FAKE_DIM);
    }

    #[test]
    fn a_backend_prefix_decides_where_a_model_runs() {
        let builtin = EmbedModel::parse("bge-small");
        assert_eq!(builtin.backend, EmbedBackend::Builtin);
        assert_eq!(builtin.canonical(), "bge-small");
        assert!(!builtin.is_remote());
        assert_eq!(builtin.known_dim(), Some(384));

        // Ollama tags carry their own colon; only the first one splits.
        let ollama = EmbedModel::parse("ollama:nomic-embed-text:latest");
        assert_eq!(ollama.backend, EmbedBackend::Ollama);
        assert_eq!(ollama.name, "nomic-embed-text:latest");
        assert_eq!(ollama.canonical(), "ollama:nomic-embed-text:latest");
        assert!(!ollama.is_remote(), "the local daemon is not remote");
        assert_eq!(ollama.known_dim(), None, "only a probe can answer");

        let remote = EmbedModel::parse("zen:text-embedding-3-small");
        assert_eq!(remote.backend, EmbedBackend::Provider("zen".to_string()));
        assert_eq!(remote.name, "text-embedding-3-small");
        assert!(remote.is_remote(), "note text would leave the machine");

        // Every form survives a round trip through its config spelling.
        for text in [
            "bge-small",
            "ollama:nomic-embed-text:latest",
            "zen:text-embedding-3-small",
        ] {
            assert_eq!(EmbedModel::parse(text).canonical(), text);
        }
        assert_eq!(EmbedModel::default().canonical(), "bge-small");
    }

    /// The aliases are a convenience, not the list of what is allowed.
    #[test]
    fn any_fastembed_name_resolves_even_without_an_alias() {
        for (alias, model) in ALIASES {
            assert_eq!(builtin_model(alias).expect("alias"), *model);
            assert_eq!(builtin_name(model.clone()), *alias);
        }
        // Named the way fastembed spells it, with no alias in sight.
        let canonical = EmbeddingModel::AllMiniLML12V2.to_string();
        assert!(!ALIASES.iter().any(|(alias, _)| *alias == canonical));
        assert_eq!(
            builtin_model(&canonical).expect("canonical name"),
            EmbeddingModel::AllMiniLML12V2
        );
        assert!(builtin_model("bge-enormous").is_none());
    }

    /// The picker offers the whole catalog, aliased where that reads better.
    #[test]
    fn the_catalog_covers_every_model_fastembed_ships() {
        let catalog = builtin_catalog();
        assert_eq!(catalog.len(), TextEmbedding::list_supported_models().len());
        assert!(catalog.len() > ALIASES.len(), "aliases are not the limit");
        assert!(catalog.iter().any(|option| option.id == "bge-small"));
        for option in &catalog {
            assert!(!option.remote);
            assert!(option.dim.is_some_and(|dim| dim > 0), "{option:?}");
            assert!(!option.description.is_empty(), "{option:?}");
            assert_eq!(
                EmbedModel::parse(&option.id).backend,
                EmbedBackend::Builtin,
                "{option:?} must round-trip as a builtin"
            );
        }
    }

    /// Downloads the real model on first run. Run manually:
    /// `cargo test -p odyn-core live_fastembed_smoke -- --ignored --nocapture`
    #[test]
    #[ignore = "downloads the bge model from the network"]
    fn live_fastembed_smoke() {
        let cache_dir = embedding_cache_dir().expect("cache dir");
        let mut embedder =
            FastEmbedder::load(EmbeddingModel::BGESmallENV15, &cache_dir).expect("load");
        let embeddings = embedder
            .embed(&[
                "What is the capital of France?",
                "Paris is the capital and largest city of France.",
                "The borrow checker rejects overlapping mutable references.",
            ])
            .expect("embed");
        let cosine = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        assert!(cosine(&embeddings[0], &embeddings[1]) > cosine(&embeddings[0], &embeddings[2]));
    }

    /// Talks to a local Ollama. Run manually with the daemon up:
    /// `cargo test -p odyn-core live_ollama_embed -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a running ollama with an embedding model pulled"]
    fn live_ollama_embed() {
        let mut embedder =
            HttpEmbedder::ollama("http://localhost:11434", "nomic-embed-text").expect("build");
        let dim = probe_dim(&mut embedder).expect("probe");
        assert!(dim > 0, "dim {dim}");
        let embeddings = embedder.embed(&["one", "two"]).expect("embed");
        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), dim);
    }
}
