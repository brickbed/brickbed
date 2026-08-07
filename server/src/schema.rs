use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::jwt::AuthConfig;
use crate::rules::CollectionRules;

/// Wire shape of `defineSchema` output pushed by clients:
/// `{ "auth": {providers: [...]}, "collections": { name: { fields: {..validators},
/// indexes: [{name, fields}], searchIndexes: [{name, fields}],
/// vectorIndexes: [{name, field, metric, dims}], rules: {read, write} } } }`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSchema {
    pub collections: BTreeMap<String, CollectionSchema>,
    /// End-user identity providers. Absent means JWTs are never accepted and
    /// auth behaves exactly as it did before this existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionSchema {
    pub fields: BTreeMap<String, Value>,
    #[serde(default)]
    pub indexes: Vec<IndexDef>,
    /// BM25 indexes: `fields` are the document fields whose text is indexed.
    #[serde(default, rename = "searchIndexes")]
    pub search_indexes: Vec<SearchIndexDef>,
    /// Brute-force vector indexes over a `v.vector(dims)` field.
    #[serde(default, rename = "vectorIndexes")]
    pub vector_indexes: Vec<VectorIndexDef>,
    /// End-user access rules. Absent means API keys only, which is what every
    /// collection did before rules existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<CollectionRules>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchIndexDef {
    pub name: String,
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VectorIndexDef {
    pub name: String,
    /// Document field holding the vector.
    pub field: String,
    #[serde(default)]
    pub metric: Metric,
    /// Must match the dimensionality declared by the field's validator.
    pub dims: u32,
}

/// Similarity metric. Both are "higher is better" so scores rank the same way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    #[default]
    Cosine,
    Dot,
}

impl ProjectSchema {
    pub fn collection(&self, name: &str) -> Option<&CollectionSchema> {
        self.collections.get(name)
    }

    /// Rules for a collection, if it declares any.
    pub fn rules(&self, collection: &str) -> Option<&CollectionRules> {
        self.collection(collection).and_then(|c| c.rules.as_ref())
    }
}

impl CollectionSchema {
    pub fn index(&self, name: &str) -> Option<&IndexDef> {
        self.indexes.iter().find(|i| i.name == name)
    }

    pub fn search_index(&self, name: &str) -> Option<&SearchIndexDef> {
        self.search_indexes.iter().find(|i| i.name == name)
    }

    pub fn vector_index(&self, name: &str) -> Option<&VectorIndexDef> {
        self.vector_indexes.iter().find(|i| i.name == name)
    }
}
