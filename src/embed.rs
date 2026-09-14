//! Optional semantic similarity via embeddings. Everything in here degrades, never fails.
//!
//! Embeddings are a nice-to-have on top of the lexical similarity in [`crate::cluster`]: a
//! story grouping that works without any model must not break because one is missing. So
//! [`EmbeddingProvider::embed`] may return [`EmbedError`], [`NullProvider`] always does, and
//! every caller is expected to fall back to token overlap when it does. The vector helpers
//! below guard the same way — [`cosine`] answers `0.0`, not `NaN`, because a NaN that leaks
//! into a score silently reorders every comparison downstream and is miserable to debug.
//!
//! There is deliberately no HTTP client here. Wiring a real provider needs an API key and a
//! network stack, and neither belongs to this boundary; whoever supplies a key plugs in a
//! provider that implements the trait.

/// Source of embedding vectors. One call, one batch; callers cache by [`EmbeddingProvider::model`].
pub trait EmbeddingProvider: Send + Sync {
    /// Stable identifier of the model, used as the cache key.
    fn model(&self) -> &str;
    /// One vector per input text, in input order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// Every way the embedding boundary can disappoint a caller. None of these are fatal: the
/// intended reaction is always to fall back to lexical similarity, not to fail the poll.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("no embedding provider is configured")]
    Unavailable,
    #[error("embedding provider returned {got} vectors for {want} inputs")]
    Count { got: usize, want: usize },
    #[error("embedding vector has {got} dimensions, expected {want}")]
    Dimension { got: usize, want: usize },
}

/// The embedding backfill job crosses two boundaries, so it needs both failures in one type.
/// The caller is expected to log and carry on: a database without vectors still ranks.
#[derive(Debug, thiserror::Error)]
pub enum EmbedJobError {
    #[error(transparent)]
    Provider(#[from] EmbedError),
    #[error(transparent)]
    Store(#[from] crate::error::StoreError),
}

/// Cosine similarity in 0..=1 for non-negative components; 0.0 when either vector is
/// empty, the lengths differ, or either norm is zero.
///
/// The zero answers are the point: `0.0` means "no evidence of similarity" and slots into
/// a score alongside the lexical term, whereas `NaN` would poison every sort it touches.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut norm_a, mut norm_b) = (0.0f32, 0.0f32, 0.0f32);
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (f64::from(dot) / (f64::from(norm_a) * f64::from(norm_b)).sqrt()).clamp(0.0, 1.0)
}

/// Unit-normalised form of a vector sum. `None` when the sum is empty or zero.
///
/// Split out from [`centroid`] because a cluster that grows one item at a time accumulates a
/// running sum instead of holding one vector per member, and needs the same normalisation at
/// the end. An unnormalised mean drifts toward shorter vectors, and [`cosine`] against a member
/// would then read as less similar than it really is.
pub fn unit(sum: &[f32]) -> Option<Vec<f32>> {
    if sum.is_empty() {
        return None;
    }
    let norm: f32 = sum.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm == 0.0 {
        return None;
    }
    Some(sum.iter().map(|x| x / norm).collect())
}

/// Component-wise mean of the members, renormalised to unit length. `None` when empty.
///
/// Renormalising is not cosmetic: an unnormalised mean has length below 1 whenever members
/// disagree, and [`cosine`] of the centroid against a member would then read as less
/// similar than any member really is. Both sides must live on the same unit sphere for the
/// comparison to mean anything.
pub fn centroid(vectors: &[Vec<f32>]) -> Option<Vec<f32>> {
    let first = vectors.first()?;
    let mut mean = vec![0.0f32; first.len()];
    for vector in vectors {
        for (acc, x) in mean.iter_mut().zip(vector) {
            *acc += x;
        }
    }
    unit(&mean)
}

/// Little-endian f32 bytes, the on-disk form in the SQLite cache.
pub fn to_bytes(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Inverse of `to_bytes`; `None` when the length is not a whole number of f32s.
///
/// A ragged tail would otherwise be dropped and the cached vector would silently shrink by
/// up to three dimensions — a corrupt cache must surface as `None`, not as short vectors.
pub fn from_bytes(bytes: &[u8]) -> Option<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    // `chunks` rather than `chunks_exact`: the length is already known to be a multiple of four,
    // so every chunk converts, and the panic branch a `try_into().expect(..)` would need does not
    // exist.
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks(4) {
        out.push(f32::from_le_bytes(match chunk.try_into() {
            Ok(word) => word,
            Err(_) => return None,
        }));
    }
    Some(out)
}

/// The default provider: always `Err(EmbedError::Unavailable)`, so every caller degrades
/// to lexical similarity instead of failing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullProvider;

impl EmbeddingProvider for NullProvider {
    fn model(&self) -> &str {
        "null"
    }

    fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Err(EmbedError::Unavailable)
    }
}

/// Embed every item that has no vector for `model` yet, newest first, up to `limit` per call.
///
/// This is the writer the cache needs. It runs after a poll, never inside a scoring pass, so
/// no refresh ever waits on a provider and no item is embedded twice: the query selects only
/// rows the cache does not already hold.
///
/// `limit` bounds one call. A database that has been running for a year holds far more items
/// than one cycle should try to embed, and the newest rows are the ones the short windows
/// rank on, so the backlog drains from the useful end.
pub fn embed_pending(
    store: &mut crate::store::Store,
    provider: &dyn EmbeddingProvider,
    limit: usize,
    now: i64,
) -> Result<usize, EmbedJobError> {
    let model = provider.model().to_string();
    let pending = store.items_missing_embeddings(&model, limit)?;
    if pending.is_empty() {
        return Ok(0);
    }
    let texts: Vec<String> = pending.iter().map(|(_, text)| text.clone()).collect();
    let vectors = provider.embed(&texts)?;
    if vectors.len() != texts.len() {
        return Err(EmbedError::Count {
            got: vectors.len(),
            want: texts.len(),
        }
        .into());
    }
    let rows: Vec<(i64, Vec<f32>)> = pending
        .iter()
        .map(|(item_id, _)| *item_id)
        .zip(vectors)
        .collect();
    Ok(store.save_embeddings(&model, &rows, now)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_of_identical_vectors_is_one() {
        assert_eq!(cosine(&[3.0, 4.0], &[3.0, 4.0]), 1.0);
    }

    #[test]
    fn cosine_is_zero_for_dimension_mismatch() {
        assert_eq!(cosine(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn cosine_is_zero_for_empty_vector() {
        assert_eq!(cosine(&[], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[1.0, 0.0], &[]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn cosine_is_zero_for_zero_norm() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
        assert_eq!(cosine(&[1.0, 0.0], &[0.0, 0.0]), 0.0);
        assert_eq!(cosine(&[0.0, 0.0], &[0.0, 0.0]), 0.0);
    }

    #[test]
    fn centroid_of_one_vector_is_that_vector() {
        let v = vec![0.6, 0.8];
        assert_eq!(centroid(std::slice::from_ref(&v)), Some(v));
    }

    #[test]
    fn centroid_of_agreeing_unit_vectors_is_the_same_unit_vector() {
        let v = vec![0.6, 0.8];
        let got = centroid(&[v.clone(), v]).unwrap();
        assert!((got[0] - 0.6).abs() < 1e-6);
        assert!((got[1] - 0.8).abs() < 1e-6);
    }

    #[test]
    fn centroid_of_empty_slice_is_none() {
        let vectors: Vec<Vec<f32>> = Vec::new();
        assert_eq!(centroid(&vectors), None);
    }

    #[test]
    fn to_bytes_from_bytes_round_trip() {
        let v = vec![1.5f32, -2.25, 0.0, f32::INFINITY];
        assert_eq!(from_bytes(&to_bytes(&v)), Some(v));
    }

    #[test]
    fn from_bytes_rejects_ragged_tail() {
        assert_eq!(from_bytes(&[1, 2, 3]), None);
    }

    #[test]
    fn null_provider_never_claims_to_work() {
        let provider = NullProvider;
        let texts = vec!["hello".to_string()];
        // `matches!` rather than `assert_eq!`: `EmbedError` carries no `PartialEq`, and it should
        // not gain one just so a test can compare it.
        assert!(
            matches!(provider.embed(&texts), Err(EmbedError::Unavailable)),
            "NullProvider must degrade to lexical similarity, never pretend to embed"
        );
    }

    /// A provider with a fixed answer and a call counter. Counting calls is the only way to
    /// prove the cache did its job: a test that only checked the stored rows would pass just as
    /// well against a provider that was asked twice.
    struct CountingProvider {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingProvider {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl EmbeddingProvider for CountingProvider {
        fn model(&self) -> &str {
            "test-model"
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(texts.iter().map(|_| vec![1.0, 0.0, 0.0]).collect())
        }
    }

    /// Answers with fewer vectors than it was asked about, which is what a truncated or
    /// mis-specified response looks like.
    struct ShortProvider;

    impl EmbeddingProvider for ShortProvider {
        fn model(&self) -> &str {
            "test-model"
        }

        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(Vec::new())
        }
    }

    fn store_with_items(count: usize) -> crate::store::Store {
        let mut store = crate::store::Store::open_in_memory().unwrap();
        let source_id = store
            .ensure_source(
                &crate::source::SourceSpec {
                    kind: crate::source::SourceKind::Rss,
                    outlet: "APA".into(),
                    name: "APA RSS".into(),
                    locator: "https://apa.az/rss".into(),
                },
                true,
            )
            .unwrap();
        let items: Vec<crate::source::ParsedItem> = (0..count)
            .map(|index| crate::source::ParsedItem {
                external_id: format!("e{index}"),
                url: format!("https://apa.az/{index}"),
                title: format!("Xəbər {index}"),
                description: None,
                section: None,
                published_at: 1_000,
                views: None,
                cited: false,
                cited_outlet: None,
                publisher: None,
            })
            .collect();
        store.upsert_items(source_id, &items, 1_000).unwrap();
        store
    }

    #[test]
    fn the_backfill_job_fills_the_cache_once_and_does_not_ask_twice() {
        let mut store = store_with_items(3);
        let provider = CountingProvider::new();

        assert_eq!(embed_pending(&mut store, &provider, 10, 2_000).unwrap(), 3);
        assert_eq!(
            embed_pending(&mut store, &provider, 10, 3_000).unwrap(),
            0,
            "every item already has a vector"
        );
        assert_eq!(
            provider.calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the second run must not reach the provider at all"
        );
        assert_eq!(
            store.current_embedding_model().unwrap().as_deref(),
            Some("test-model"),
            "the cache names the model it holds"
        );
    }

    #[test]
    fn a_run_with_nothing_to_embed_never_calls_the_provider() {
        let mut store = store_with_items(0);
        let provider = CountingProvider::new();
        assert_eq!(embed_pending(&mut store, &provider, 10, 2_000).unwrap(), 0);
        assert_eq!(provider.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert_eq!(
            store.current_embedding_model().unwrap(),
            None,
            "an empty cache has no model to name"
        );
    }

    #[test]
    fn a_provider_that_returns_the_wrong_number_of_vectors_is_an_error() {
        let mut store = store_with_items(2);
        let error = embed_pending(&mut store, &ShortProvider, 10, 2_000).unwrap_err();
        assert!(
            matches!(
                error,
                EmbedJobError::Provider(EmbedError::Count { got: 0, want: 2 })
            ),
            "a truncated answer must not be stored against the wrong items: {error}"
        );
        assert!(
            store
                .embeddings_for_range(0, 10_000, "test-model")
                .unwrap()
                .is_empty(),
            "nothing is written when the answer does not line up"
        );
    }

    #[test]
    fn the_limit_bounds_one_call_and_the_backlog_is_caught_up_over_many() {
        let mut store = store_with_items(5);
        let provider = CountingProvider::new();
        assert_eq!(embed_pending(&mut store, &provider, 2, 2_000).unwrap(), 2);
        assert_eq!(embed_pending(&mut store, &provider, 2, 2_001).unwrap(), 2);
        assert_eq!(embed_pending(&mut store, &provider, 2, 2_002).unwrap(), 1);
        assert_eq!(embed_pending(&mut store, &provider, 2, 2_003).unwrap(), 0);
    }
}
