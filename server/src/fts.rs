//! Full-text search: tokenizer, `_fts` key encoding, BM25 scoring.
//!
//! Key layout (shares the escape scheme in `index.rs`, so an encoded term never
//! contains 0x00 and a scan on `enc(term) 0x00` matches that term only):
//!
//! ```text
//! _fts:{project}:{collection}:{index}:t:enc(term) 0x00 {docid}  -> tf:u32-LE doc_len:u32-LE
//! _fts:{project}:{collection}:{index}:s                          -> msgpack Stats
//! ```
//!
//! Storing `doc_len` in every posting makes a document scorable from one scan
//! per query term plus one stats read; document frequency is counted during
//! that same scan.

use std::collections::HashMap;
use std::sync::OnceLock;

use rust_stemmers::{Algorithm, Stemmer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use unicode_segmentation::UnicodeSegmentation;

use crate::error::AppError;
use crate::index::{escape_into, SEP};
use crate::schema::SearchIndexDef;

/// BM25 term-frequency saturation.
const K1: f64 = 1.2;
/// BM25 length normalisation.
const B: f64 = 0.75;

/// Stemmed tokens longer than this are dropped: real words are shorter and it
/// keeps posting keys bounded.
const MAX_TERM_LEN: usize = 64;

/// Classic english stop list. Applied before stemming.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "if", "in", "into", "is", "it",
    "no", "not", "of", "on", "or", "such", "that", "the", "their", "then", "there", "these",
    "they", "this", "to", "was", "will", "with",
];

fn stemmer() -> &'static Stemmer {
    static STEMMER: OnceLock<Stemmer> = OnceLock::new();
    STEMMER.get_or_init(|| Stemmer::create(Algorithm::English))
}

/// lowercase -> unicode word segmentation -> stopword filter -> snowball stem.
pub fn tokenize(text: &str) -> Vec<String> {
    let lowered = text.to_lowercase();
    lowered
        .unicode_words()
        .filter(|w| !STOPWORDS.contains(w))
        .map(|w| stemmer().stem(w).into_owned())
        .filter(|t| !t.is_empty() && t.len() <= MAX_TERM_LEN)
        .collect()
}

/// Query terms, deduplicated but in query order.
pub fn query_terms(query: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for term in tokenize(query) {
        if !seen.contains(&term) {
            seen.push(term);
        }
    }
    seen
}

/// Indexable text of a document field. Strings and (nested) arrays of strings
/// contribute; numbers, booleans and objects are ignored.
fn collect_text(value: &Value, out: &mut String) {
    match value {
        Value::String(s) => {
            out.push_str(s);
            out.push(' ');
        }
        Value::Array(items) => {
            for item in items {
                collect_text(item, out);
            }
        }
        _ => {}
    }
}

/// Term frequencies and length of one document under one search index.
#[derive(Debug, Default)]
pub struct DocTerms {
    pub tf: HashMap<String, u32>,
    pub len: u32,
}

pub fn doc_terms(index: &SearchIndexDef, data: &Map<String, Value>) -> DocTerms {
    let mut text = String::new();
    for field in &index.fields {
        if let Some(value) = data.get(field) {
            collect_text(value, &mut text);
        }
    }

    let mut terms = DocTerms::default();
    for token in tokenize(&text) {
        *terms.tf.entry(token).or_insert(0) += 1;
        terms.len = terms.len.saturating_add(1);
    }
    terms
}

/// Corpus statistics for one search index, used for IDF and average length.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct Stats {
    pub n_docs: u64,
    pub total_len: u64,
}

impl Stats {
    pub fn add_doc(&mut self, doc_len: u32) {
        self.n_docs = self.n_docs.saturating_add(1);
        self.total_len = self.total_len.saturating_add(doc_len as u64);
    }

    pub fn remove_doc(&mut self, doc_len: u32) {
        self.n_docs = self.n_docs.saturating_sub(1);
        self.total_len = self.total_len.saturating_sub(doc_len as u64);
    }

    /// Average document length, never zero (avoids a divide-by-zero in scoring
    /// when every indexed document is empty).
    pub fn avg_len(&self) -> f64 {
        if self.n_docs == 0 || self.total_len == 0 {
            return 1.0;
        }
        self.total_len as f64 / self.n_docs as f64
    }
}

pub fn encode_stats(stats: &Stats) -> Result<Vec<u8>, AppError> {
    rmp_serde::to_vec(stats).map_err(|e| AppError::Internal(format!("Stats encode error: {}", e)))
}

pub fn decode_stats(bytes: &[u8]) -> Result<Stats, AppError> {
    rmp_serde::from_slice(bytes)
        .map_err(|e| AppError::Internal(format!("Stats decode error: {}", e)))
}

fn index_base(project: &str, collection: &str, index_name: &str) -> Vec<u8> {
    format!("_fts:{}:{}:{}:", project, collection, index_name).into_bytes()
}

/// Prefix for all search entries of a project (used for backfill cleanup).
pub fn project_prefix(project: &str) -> Vec<u8> {
    format!("_fts:{}:", project).into_bytes()
}

pub fn stats_key(project: &str, collection: &str, index_name: &str) -> Vec<u8> {
    let mut key = index_base(project, collection, index_name);
    key.push(b's');
    key
}

/// Scan prefix for every posting of one term.
pub fn term_prefix(project: &str, collection: &str, index_name: &str, term: &str) -> Vec<u8> {
    let mut key = index_base(project, collection, index_name);
    key.extend_from_slice(b"t:");
    escape_into(term.as_bytes(), &mut key);
    key.push(SEP);
    key
}

pub fn posting_key(
    project: &str,
    collection: &str,
    index_name: &str,
    term: &str,
    id: &str,
) -> Vec<u8> {
    let mut key = term_prefix(project, collection, index_name, term);
    key.extend_from_slice(id.as_bytes());
    key
}

/// Document id of a posting key produced by `posting_key` for `prefix`.
pub fn posting_doc_id(key: &[u8], term_prefix: &[u8]) -> Option<String> {
    let rest = key.strip_prefix(term_prefix)?;
    std::str::from_utf8(rest).ok().map(str::to_string)
}

pub fn encode_posting(tf: u32, doc_len: u32) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&tf.to_le_bytes());
    out[4..].copy_from_slice(&doc_len.to_le_bytes());
    out
}

pub fn decode_posting(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() != 8 {
        return None;
    }
    let tf = u32::from_le_bytes(bytes[..4].try_into().ok()?);
    let doc_len = u32::from_le_bytes(bytes[4..].try_into().ok()?);
    Some((tf, doc_len))
}

/// Lucene-style IDF: always positive, even for terms in most documents.
pub fn idf(n_docs: u64, doc_freq: u64) -> f64 {
    let n = n_docs as f64;
    let df = doc_freq as f64;
    ((n - df + 0.5) / (df + 0.5) + 1.0).ln()
}

pub fn bm25_term_score(idf: f64, tf: u32, doc_len: u32, avg_len: f64) -> f64 {
    let tf = tf as f64;
    let norm = 1.0 - B + B * (doc_len as f64 / avg_len);
    idf * (tf * (K1 + 1.0)) / (tf + K1 * norm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tokenizer_lowercases_stems_and_drops_stopwords() {
        assert_eq!(
            tokenize("The Running Dogs are Jumping"),
            vec!["run", "dog", "jump"]
        );
    }

    #[test]
    fn tokenizer_splits_on_punctuation_and_keeps_unicode() {
        // UAX#29 keeps dotted words like domains whole; trailing punctuation goes.
        assert_eq!(tokenize("fly.io deploys!"), vec!["fly.io", "deploy"]);
        assert_eq!(tokenize("Café Crème"), vec!["café", "crème"]);
    }

    #[test]
    fn query_terms_deduplicate_in_order() {
        assert_eq!(query_terms("deploy deploying deploys"), vec!["deploy"]);
        assert_eq!(query_terms(""), Vec::<String>::new());
    }

    #[test]
    fn doc_terms_counts_across_fields_and_arrays() {
        let index = SearchIndexDef {
            name: "search".to_string(),
            fields: vec!["title".to_string(), "tags".to_string()],
        };
        let data = json!({
            "title": "Rust rust database",
            "tags": ["rust", "storage"],
            "ignored": "elsewhere"
        });
        let terms = doc_terms(&index, data.as_object().unwrap());
        assert_eq!(terms.tf.get("rust"), Some(&3));
        assert_eq!(terms.tf.get("storag"), Some(&1));
        assert_eq!(terms.tf.get("elsewher"), None);
        assert_eq!(terms.len, 5);
    }

    #[test]
    fn posting_roundtrip() {
        let prefix = term_prefix("proj", "posts", "search", "rust");
        let key = posting_key("proj", "posts", "search", "rust", "01ABC");
        assert!(key.starts_with(&prefix));
        assert_eq!(posting_doc_id(&key, &prefix).as_deref(), Some("01ABC"));
        assert_eq!(decode_posting(&encode_posting(3, 42)), Some((3, 42)));
        assert_eq!(decode_posting(&[0, 1, 2]), None);
    }

    #[test]
    fn term_prefix_does_not_match_longer_terms() {
        let short = term_prefix("proj", "posts", "search", "rust");
        let long = posting_key("proj", "posts", "search", "rustacean", "01ABC");
        assert!(!long.starts_with(&short));
    }

    #[test]
    fn stats_roundtrip_and_averages() {
        let mut stats = Stats::default();
        stats.add_doc(10);
        stats.add_doc(20);
        stats.remove_doc(10);
        assert_eq!(stats.n_docs, 1);
        assert_eq!(stats.total_len, 20);
        assert_eq!(stats.avg_len(), 20.0);
        assert_eq!(Stats::default().avg_len(), 1.0);

        let decoded = decode_stats(&encode_stats(&stats).unwrap()).unwrap();
        assert_eq!(decoded.n_docs, 1);
        assert_eq!(decoded.total_len, 20);
    }

    #[test]
    fn idf_stays_positive_for_common_terms() {
        assert!(idf(10, 10) > 0.0);
        assert!(idf(10, 1) > idf(10, 9));
    }

    #[test]
    fn score_saturates_with_tf_and_penalises_long_docs() {
        let idf = idf(100, 10);
        let short = bm25_term_score(idf, 2, 10, 10.0);
        let long = bm25_term_score(idf, 2, 100, 10.0);
        assert!(short > long, "long documents must score lower");

        let once = bm25_term_score(idf, 1, 10, 10.0);
        let twice = bm25_term_score(idf, 2, 10, 10.0);
        assert!(twice > once && twice < 2.0 * once, "tf must saturate");
    }
}
