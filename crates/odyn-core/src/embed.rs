//! Text embeddings for episodic memory retrieval.
//!
//! The model is bge-small-en-v1.5 via fastembed: 384 dimensions, downloaded
//! into the data dir on first use with fastembed's own progress line. Nothing
//! holds a loaded model between batches — callers construct a [`FastEmbedder`],
//! embed, and drop it, so the ~100MB of weights never sit resident.

use std::path::{Path, PathBuf};

use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};

pub const EMBEDDING_DIM: usize = 384;

const MODEL_CACHE_DIR_NAME: &str = "models";

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("could not load the embedding model: {0}")]
    Load(String),
    #[error("embedding failed: {0}")]
    Embed(String),
    #[error("could not locate a data directory for the embedding model cache")]
    NoDataDir,
}

pub trait Embedder {
    /// One vector per input text, in input order, each [`EMBEDDING_DIM`] long.
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

pub struct FastEmbedder {
    model: TextEmbedding,
}

impl FastEmbedder {
    /// Loads the model, downloading it into `cache_dir` the first time.
    pub fn load(cache_dir: &Path) -> Result<Self, EmbedError> {
        let options = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_cache_dir(cache_dir.to_path_buf())
            .with_show_download_progress(true);
        let model =
            TextEmbedding::try_new(options).map_err(|error| EmbedError::Load(error.to_string()))?;
        Ok(Self { model })
    }
}

impl Embedder for FastEmbedder {
    fn embed(&mut self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let embeddings = self
            .model
            .embed(texts, None)
            .map_err(|error| EmbedError::Embed(error.to_string()))?;
        for embedding in &embeddings {
            if embedding.len() != EMBEDDING_DIM {
                return Err(EmbedError::Embed(format!(
                    "the model returned {}-dimensional embeddings, expected {EMBEDDING_DIM}",
                    embedding.len()
                )));
            }
        }
        Ok(embeddings)
    }
}

/// Where model files live: the platform data dir, next to the database.
pub fn embedding_cache_dir() -> Result<PathBuf, EmbedError> {
    let dirs = directories::ProjectDirs::from("", "", "odyn").ok_or(EmbedError::NoDataDir)?;
    Ok(dirs.data_dir().join(MODEL_CACHE_DIR_NAME))
}

/// The loader `brain::build_context` callers pass: the real model from the
/// default cache dir, loaded only if retrieval actually needs it.
pub fn load_default_embedder() -> Result<Box<dyn Embedder>, EmbedError> {
    Ok(Box::new(FastEmbedder::load(&embedding_cache_dir()?)?))
}

/// Deterministic stand-in for tests: an LCG seeded from the text's bytes,
/// L2-normalized like the real model's output.
#[cfg(test)]
pub(crate) struct FakeEmbedder;

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
    let mut values: Vec<f32> = (0..EMBEDDING_DIM)
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
            assert_eq!(embedding.len(), EMBEDDING_DIM);
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

    /// Downloads the real model on first run. Run manually:
    /// `cargo test -p odyn-core live_fastembed_smoke -- --ignored --nocapture`
    #[test]
    #[ignore = "downloads the bge model from the network"]
    fn live_fastembed_smoke() {
        let cache_dir = embedding_cache_dir().expect("cache dir");
        let mut embedder = FastEmbedder::load(&cache_dir).expect("load");
        let embeddings = embedder
            .embed(&[
                "What is the capital of France?",
                "Paris is the capital and largest city of France.",
                "The borrow checker rejects overlapping mutable references.",
            ])
            .expect("embed");
        let cosine = |a: &[f32], b: &[f32]| -> f32 { a.iter().zip(b).map(|(x, y)| x * y).sum() };
        let related = cosine(&embeddings[0], &embeddings[1]);
        let unrelated = cosine(&embeddings[0], &embeddings[2]);
        assert!(
            related > unrelated,
            "related {related} should beat unrelated {unrelated}"
        );
    }
}
