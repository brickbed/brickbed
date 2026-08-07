//! Vector search: `_vec` key encoding, similarity scoring, brute-force top-k.
//!
//! Key layout:
//!
//! ```text
//! _vec:{project}:{collection}:{index}:{docid} -> f32-LE components
//! ```
//!
//! One entry per document per vector index, keyed by document id alone: a
//! write replaces (or deletes) an entry without having to know the vector it
//! held. Document ids are validated to exclude `:`, so the id is the whole
//! suffix after the index prefix.

use std::cmp::{Ordering, Reverse};
use std::collections::BinaryHeap;

use serde_json::{Map, Value};

use crate::schema::Metric;

/// Upper bound on vector dimensionality. Brute-force search keeps every vector
/// of a collection in memory, so this caps how much one document can cost.
pub const MAX_DIMS: u32 = 4096;

fn index_base(project: &str, collection: &str, index_name: &str) -> Vec<u8> {
    format!("_vec:{}:{}:{}:", project, collection, index_name).into_bytes()
}

/// Scan prefix for every vector of one index.
pub fn index_prefix(project: &str, collection: &str, index_name: &str) -> Vec<u8> {
    index_base(project, collection, index_name)
}

/// Prefix for all vector entries of a project (used for backfill cleanup).
pub fn project_prefix(project: &str) -> Vec<u8> {
    format!("_vec:{}:", project).into_bytes()
}

pub fn entry_key(project: &str, collection: &str, index_name: &str, id: &str) -> Vec<u8> {
    let mut key = index_base(project, collection, index_name);
    key.extend_from_slice(id.as_bytes());
    key
}

/// Document id of an entry key produced by `entry_key` for `prefix`.
pub fn entry_doc_id(key: &[u8], prefix: &[u8]) -> Option<String> {
    let rest = key.strip_prefix(prefix)?;
    std::str::from_utf8(rest).ok().map(str::to_string)
}

pub fn encode(vector: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(vector.len() * 4);
    for component in vector {
        out.extend_from_slice(&component.to_le_bytes());
    }
    out
}

pub fn decode(bytes: &[u8]) -> Option<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect(),
    )
}

/// The vector a document contributes to an index, or `None` if the field is
/// absent or does not hold exactly `dims` finite numbers. Documents written
/// before the schema declared the field fall in the `None` case, so a bad
/// value is skipped rather than stored at the wrong width.
pub fn doc_vector(data: &Map<String, Value>, field: &str, dims: usize) -> Option<Vec<f32>> {
    let items = data.get(field)?.as_array()?;
    if items.len() != dims {
        return None;
    }
    let mut out = Vec::with_capacity(dims);
    for item in items {
        let component = item.as_f64()? as f32;
        if !component.is_finite() {
            return None;
        }
        out.push(component);
    }
    Some(out)
}

/// Scores accumulate in f64 so that vectors near the top of the f32 range
/// cannot overflow to infinity mid-sum: every score of a storable vector stays
/// finite, and so serializable as JSON.
pub fn dot(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum()
}

/// Cosine similarity in [-1, 1]. A zero vector has no direction, so it scores
/// 0 against everything instead of producing NaN.
pub fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let mut prod = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b) {
        let (x, y) = (f64::from(*x), f64::from(*y));
        prod += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    prod / (norm_a.sqrt() * norm_b.sqrt())
}

/// Similarity under the index's metric: higher is always better.
pub fn similarity(metric: Metric, a: &[f32], b: &[f32]) -> f64 {
    match metric {
        Metric::Cosine => cosine(a, b),
        Metric::Dot => dot(a, b),
    }
}

/// The vectors of one index, in key (so document id) order.
#[derive(Debug, Default)]
pub struct VectorSet {
    pub ids: Vec<String>,
    pub vectors: Vec<Vec<f32>>,
}

impl VectorSet {
    pub fn push(&mut self, id: String, vector: Vec<f32>) {
        self.ids.push(id);
        self.vectors.push(vector);
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }
}

/// A candidate in the top-k heap. Ordered best-first: higher score wins, and
/// equal scores prefer the earlier entry (the smaller document id, since the
/// vectors come from a key-ordered scan).
#[derive(Debug, PartialEq)]
struct Candidate {
    score: f64,
    position: usize,
}

impl Eq for Candidate {}

impl Ord for Candidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .partial_cmp(&other.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.position.cmp(&self.position))
    }
}

impl PartialOrd for Candidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Brute-force top-`k` over `vectors`, best first, as `(position, score)`.
/// A bounded min-heap keeps the scan O(n log k) and allocation-free.
pub fn top_k(metric: Metric, query: &[f32], vectors: &[Vec<f32>], k: usize) -> Vec<(usize, f64)> {
    if k == 0 {
        return Vec::new();
    }
    let mut heap: BinaryHeap<Reverse<Candidate>> = BinaryHeap::with_capacity(k + 1);
    for (position, vector) in vectors.iter().enumerate() {
        let score = similarity(metric, query, vector);
        heap.push(Reverse(Candidate { score, position }));
        if heap.len() > k {
            heap.pop();
        }
    }
    // Ascending order of `Reverse<Candidate>` is descending order of the
    // candidates themselves, so the best hit comes out first.
    heap.into_sorted_vec()
        .into_iter()
        .map(|Reverse(c)| (c.position, c.score))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vector_roundtrip() {
        let vector = vec![1.0, -0.5, 2.25];
        assert_eq!(decode(&encode(&vector)).unwrap(), vector);
        assert_eq!(encode(&vector).len(), 12);
        assert_eq!(decode(&[0, 1, 2]), None);
        assert_eq!(decode(&[]), Some(Vec::new()));
    }

    #[test]
    fn entry_key_roundtrip() {
        let prefix = index_prefix("proj", "posts", "by_embedding");
        let key = entry_key("proj", "posts", "by_embedding", "01ABC");
        assert!(key.starts_with(&prefix));
        assert_eq!(entry_doc_id(&key, &prefix).as_deref(), Some("01ABC"));
        assert_eq!(entry_doc_id(b"_vec:other:", &prefix), None);
    }

    #[test]
    fn doc_vector_requires_exact_dims_and_finite_numbers() {
        let data = json!({
            "embedding": [1, 2.5, -3],
            "short": [1.0, 2.0],
            "text": "nope",
            "mixed": [1.0, "two", 3.0],
            "huge": [1e39, 0.0, 0.0]
        });
        let data = data.as_object().unwrap();

        assert_eq!(doc_vector(data, "embedding", 3), Some(vec![1.0, 2.5, -3.0]));
        assert_eq!(doc_vector(data, "embedding", 4), None);
        assert_eq!(doc_vector(data, "short", 3), None);
        assert_eq!(doc_vector(data, "text", 3), None);
        assert_eq!(doc_vector(data, "mixed", 3), None);
        assert_eq!(doc_vector(data, "absent", 3), None);
        // Overflows f32 on the way in, so it never reaches storage.
        assert_eq!(doc_vector(data, "huge", 3), None);
    }

    #[test]
    fn cosine_ignores_magnitude_but_dot_does_not() {
        let query = [1.0, 0.0];
        let same = [5.0, 0.0];
        assert!((cosine(&query, &same) - 1.0).abs() < 1e-6);
        assert_eq!(dot(&query, &same), 5.0);

        assert_eq!(cosine(&query, &[0.0, 1.0]), 0.0);
        assert!((cosine(&query, &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        // Zero vectors have no direction: 0 rather than NaN.
        assert_eq!(cosine(&query, &[0.0, 0.0]), 0.0);
    }

    #[test]
    fn scores_stay_finite_for_extreme_vectors() {
        // Squaring these overflows f32, which would score them as inf/NaN and
        // serialize as a null `_score`.
        let big = [1e20f32, 1e20];
        assert!(dot(&big, &big).is_finite());
        assert!((cosine(&big, &big) - 1.0).abs() < 1e-6);

        let huge = [f32::MAX, f32::MAX];
        assert!(dot(&huge, &huge).is_finite());
        assert!(cosine(&huge, &[1.0, 0.0]).is_finite());
    }

    #[test]
    fn top_k_returns_best_first_and_breaks_ties_by_position() {
        let vectors = vec![
            vec![0.0, 1.0],  // orthogonal
            vec![1.0, 0.0],  // exact
            vec![0.9, 0.1],  // close
            vec![-1.0, 0.0], // opposite
        ];
        let hits = top_k(Metric::Cosine, &[1.0, 0.0], &vectors, 3);
        assert_eq!(
            hits.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            vec![1, 2, 0]
        );
        assert!(hits.windows(2).all(|w| w[0].1 >= w[1].1));

        // Identical scores fall back to scan order.
        let tied = vec![vec![1.0, 0.0], vec![1.0, 0.0], vec![1.0, 0.0]];
        let hits = top_k(Metric::Dot, &[1.0, 0.0], &tied, 2);
        assert_eq!(hits.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![0, 1]);

        assert!(top_k(Metric::Cosine, &[1.0, 0.0], &vectors, 0).is_empty());
        assert_eq!(top_k(Metric::Cosine, &[1.0, 0.0], &vectors, 99).len(), 4);
    }
}
