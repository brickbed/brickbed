use serde_json::{Map, Value};
use std::collections::BTreeSet;

use crate::error::AppError;
use crate::rules::{Rule, WriteRule};
use crate::schema::{CollectionSchema, ProjectSchema};
use crate::vector::MAX_DIMS;

/// Top-level document fields owned by the server or synthesized in responses.
/// Allowing clients to persist any of these alongside `Document`'s flattened
/// data creates duplicate MessagePack/JSON keys and makes the record unreadable.
pub const RESERVED_DOCUMENT_FIELDS: [&str; 4] = ["_id", "_createdAt", "_updatedAt", "_score"];

/// Reject client-supplied server metadata at the database boundary. This is
/// intentionally below the HTTP handlers so direct callers cannot create an
/// unreadable flattened `Document` either.
pub fn reject_reserved_document_fields(data: &Map<String, Value>) -> Result<(), AppError> {
    if let Some(field) = RESERVED_DOCUMENT_FIELDS
        .iter()
        .find(|field| data.contains_key(**field))
    {
        return Err(AppError::BadRequest(format!(
            "field {:?} is reserved by Brickbed",
            field
        )));
    }
    Ok(())
}

/// A schema requiring a server-owned field would make every write fail (and an
/// embed-on-write vector targeting one could recreate the duplicate-key bug).
pub fn check_reserved_schema_fields(schema: &ProjectSchema) -> Result<(), AppError> {
    for (collection, coll_schema) in &schema.collections {
        if let Some(field) = RESERVED_DOCUMENT_FIELDS
            .iter()
            .find(|field| coll_schema.fields.contains_key(**field))
        {
            return Err(AppError::BadRequest(format!(
                "collection {:?}: field {:?} is reserved by Brickbed",
                collection, field
            )));
        }
    }
    Ok(())
}

/// Project and collection names: `[a-z0-9][a-z0-9_-]{0,63}`.
/// Leading `_` is rejected, which reserves the `_meta:`/`_idx:`/`_fts:`/`_vec:`
/// key namespaces and `_schema`/`_query`/`_search` route segments.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return false;
    }
    bytes[1..]
        .iter()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-' || *b == b'_')
}

/// Document ids: URL-safe, no `:` (key separator), bounded length.
pub fn valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
}

/// Index, search-index and vector-index names appear unescaped in
/// `_idx:`/`_fts:`/`_vec:` keys, so
/// they must not contain the key separators (`:` and 0x00) or other control
/// bytes: a name carrying one could make a scan match another index's entries.
/// Case and unicode are allowed, unlike collection names.
pub fn valid_index_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 64
        && bytes.iter().all(|b| *b >= 0x20 && *b != 0x7f && *b != b':')
}

/// Validate every name a pushed schema contributes to the key space. Duplicate
/// index names within a collection are rejected: they share one keyspace but
/// would be counted twice when rebuilding search corpus stats.
pub fn check_schema_names(schema: &ProjectSchema) -> Result<(), AppError> {
    for (collection, coll_schema) in &schema.collections {
        if !valid_name(collection) {
            return Err(AppError::BadRequest(format!(
                "invalid collection name in schema: {:?}",
                collection
            )));
        }

        let indexes = coll_schema.indexes.iter().map(|i| ("index", &i.name));
        let searches = coll_schema
            .search_indexes
            .iter()
            .map(|i| ("search index", &i.name));
        let vectors = coll_schema
            .vector_indexes
            .iter()
            .map(|i| ("vector index", &i.name));

        let mut seen: BTreeSet<(&str, &String)> = BTreeSet::new();
        for (kind, name) in indexes.chain(searches).chain(vectors) {
            if !valid_index_name(name) {
                return Err(AppError::BadRequest(format!(
                    "invalid {} name on {:?}: {:?}",
                    kind, collection, name
                )));
            }
            if !seen.insert((kind, name)) {
                return Err(AppError::BadRequest(format!(
                    "duplicate {} name on {:?}: {:?}",
                    kind, collection, name
                )));
            }
        }
    }
    Ok(())
}

/// Vector guardrails for a pushed schema: every declared vector width stays
/// inside `MAX_DIMS` (brute-force search holds a collection's vectors in
/// memory), and a vector index points at a declared vector field of the same
/// width — otherwise it would silently index nothing.
pub fn check_vector_indexes(schema: &ProjectSchema) -> Result<(), AppError> {
    for (collection, coll_schema) in &schema.collections {
        for (field, validator) in &coll_schema.fields {
            check_vector_dims(collection, field, validator)?;
        }

        for vidx in &coll_schema.vector_indexes {
            let declared = coll_schema
                .fields
                .get(&vidx.field)
                .map(unwrap_optional)
                .and_then(vector_dims);

            let Some(dims) = declared else {
                return Err(AppError::BadRequest(format!(
                    "vector index {:?} on {:?}: field {:?} is not a vector field",
                    vidx.name, collection, vidx.field
                )));
            };
            if dims != u64::from(vidx.dims) {
                return Err(AppError::BadRequest(format!(
                    "vector index {:?} on {:?}: declares {} dimensions but field {:?} has {}",
                    vidx.name, collection, vidx.dims, vidx.field, dims
                )));
            }
        }
    }
    Ok(())
}

/// Inner validator of an `optional`, or the validator itself.
fn unwrap_optional(validator: &Value) -> &Value {
    if validator.get("type").and_then(Value::as_str) == Some("optional") {
        return validator.get("inner").unwrap_or(validator);
    }
    validator
}

/// Declared width of a vector validator, `None` for any other type.
fn vector_dims(validator: &Value) -> Option<u64> {
    if validator.get("type").and_then(Value::as_str) != Some("vector") {
        return None;
    }
    validator.get("dims").and_then(Value::as_u64)
}

/// Reject out-of-range vector widths anywhere in a validator tree.
fn check_vector_dims(collection: &str, path: &str, validator: &Value) -> Result<(), AppError> {
    let bad = |msg: String| AppError::BadRequest(format!("{} on {:?}: {}", path, collection, msg));

    match validator.get("type").and_then(Value::as_str) {
        Some("vector") => {
            let dims = vector_dims(validator)
                .ok_or_else(|| bad("vector validator needs a numeric \"dims\"".to_string()))?;
            if dims == 0 || dims > u64::from(MAX_DIMS) {
                return Err(bad(format!(
                    "vector dims must be between 1 and {}, got {}",
                    MAX_DIMS, dims
                )));
            }
        }
        Some("optional") => {
            if let Some(inner) = validator.get("inner") {
                check_vector_dims(collection, path, inner)?;
            }
        }
        Some("array") => {
            if let Some(items) = validator.get("items") {
                check_vector_dims(collection, path, items)?;
            }
        }
        Some("object") => {
            if let Some(props) = validator.get("properties").and_then(Value::as_object) {
                for (key, prop) in props {
                    check_vector_dims(collection, &format!("{}.{}", path, key), prop)?;
                }
            }
        }
        Some("union") => {
            if let Some(variants) = validator.get("variants").and_then(Value::as_array) {
                for variant in variants {
                    check_vector_dims(collection, path, variant)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

/// Embed-on-write settings live on the vector validator, so they are checked
/// when the schema is pushed: a `from` naming a field that does not exist, or
/// one holding no text, would otherwise fail silently on every write by simply
/// never producing a vector.
pub fn check_embed_sources(schema: &ProjectSchema) -> Result<(), AppError> {
    for (collection, coll_schema) in &schema.collections {
        for (field, validator) in &coll_schema.fields {
            let vector = unwrap_optional(validator);
            let Some(from) = vector.get("from") else {
                continue;
            };
            let bad = |msg: String| {
                AppError::BadRequest(format!("{} on {:?}: {}", field, collection, msg))
            };

            if vector.get("type").and_then(Value::as_str) != Some("vector") {
                return Err(bad("\"from\" only applies to a vector field".to_string()));
            }

            let sources = from
                .as_array()
                .ok_or_else(|| bad("\"from\" must be an array of field names".to_string()))?;
            if sources.is_empty() {
                return Err(bad("\"from\" must name at least one field".to_string()));
            }

            let model = vector
                .get("model")
                .and_then(Value::as_str)
                .ok_or_else(|| bad("embedded vector needs a \"model\"".to_string()))?;
            if model.trim().is_empty() {
                return Err(bad("\"model\" must not be empty".to_string()));
            }

            for source in sources {
                let name = source
                    .as_str()
                    .ok_or_else(|| bad("\"from\" must hold field names".to_string()))?;
                if name == field {
                    return Err(bad("a vector field cannot embed itself".to_string()));
                }
                let source_validator = coll_schema
                    .fields
                    .get(name)
                    .ok_or_else(|| bad(format!("\"from\" names unknown field {:?}", name)))?;
                if !is_text_like(source_validator) {
                    return Err(bad(format!(
                        "\"from\" field {:?} holds no text to embed",
                        name
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Validate the `auth` block of a pushed schema and shout about collections the
/// push is opening to the world.
pub fn check_auth_config(schema: &ProjectSchema) -> Result<(), AppError> {
    if let Some(auth) = &schema.auth {
        crate::jwt::validate_auth_config(auth).map_err(|e| AppError::BadRequest(e.to_string()))?;
    }

    // A public collection is readable by anyone from any origin (CORS is
    // permissive by design). That is the point of the rule, but it should never
    // happen quietly.
    for (name, collection) in &schema.collections {
        let Some(rules) = &collection.rules else {
            continue;
        };
        if rules.read == Some(Rule::Public) {
            tracing::warn!(
                "collection {:?} is now publicly readable without any credential",
                name
            );
        }
        if matches!(&rules.write, Some(WriteRule::All(Rule::Public))) {
            tracing::warn!(
                "collection {:?} is now publicly WRITABLE without any credential",
                name
            );
        }
        // An owner rule must match on a field the client controls. A vector
        // field is filled by the server during the write, *after* the rule is
        // evaluated, so the document judged would not be the document stored.
        for field in rules.owner_fields() {
            let declared = collection.fields.get(field).map(unwrap_optional);
            if declared.and_then(vector_dims).is_some() {
                return Err(AppError::BadRequest(format!(
                    "collection {:?}: owner rule cannot match on {:?}, which is a \
                     server-filled vector field",
                    name, field
                )));
            }
        }

        // Rules are meaningless without a provider to authenticate against.
        let needs_identity = rules.names_an_identity_rule();
        if needs_identity && schema.auth.is_none() {
            return Err(AppError::BadRequest(format!(
                "collection {:?} has rules needing an end-user identity, but the \
                 schema declares no \"auth\" providers",
                name
            )));
        }
    }
    Ok(())
}

/// Whether a validator describes something an embedding can be built from:
/// strings, arrays of strings, and unions of those.
fn is_text_like(validator: &Value) -> bool {
    let validator = unwrap_optional(validator);
    match validator.get("type").and_then(Value::as_str) {
        Some("string" | "id") => true,
        Some("literal") => validator.get("value").is_some_and(Value::is_string),
        Some("array") => validator.get("items").is_some_and(is_text_like),
        Some("union") => validator
            .get("variants")
            .and_then(Value::as_array)
            .is_some_and(|variants| !variants.is_empty() && variants.iter().all(is_text_like)),
        _ => false,
    }
}

/// Whether a field is filled in by the server rather than the client.
fn is_server_filled(validator: &Value) -> bool {
    let validator = unwrap_optional(validator);
    validator.get("type").and_then(Value::as_str) == Some("vector")
        && validator
            .get("from")
            .and_then(Value::as_array)
            .is_some_and(|from| !from.is_empty())
}

/// Does this push change the project's trust configuration?
///
/// Rewriting `auth.providers` repoints trust at an issuer of the pusher's
/// choosing, so it is gated on an instance-wide key rather than a
/// project-scoped one. An unchanged block re-pushed alongside ordinary schema
/// edits is not a trust change and stays open to project keys.
pub fn changes_auth_config(incoming: &ProjectSchema, stored: Option<&ProjectSchema>) -> bool {
    let stored_auth = stored.and_then(|s| s.auth.as_ref());
    match (&incoming.auth, stored_auth) {
        (None, None) => false,
        (Some(a), Some(b)) => serde_json::to_value(a).ok() != serde_json::to_value(b).ok(),
        // Added or removed.
        _ => true,
    }
}

pub fn check_names(project: &str, collection: &str, id: Option<&str>) -> Result<(), AppError> {
    if !valid_name(project) {
        return Err(AppError::BadRequest(format!(
            "invalid project name: {:?}",
            project
        )));
    }
    if !valid_name(collection) {
        return Err(AppError::BadRequest(format!(
            "invalid collection name: {:?}",
            collection
        )));
    }
    if let Some(id) = id {
        if !valid_id(id) {
            return Err(AppError::BadRequest(format!("invalid id: {:?}", id)));
        }
    }
    Ok(())
}

/// Validate declared fields against their validator descriptors.
/// Undeclared extra fields are allowed (permissive MVP).
pub fn validate_doc(schema: &CollectionSchema, data: &Map<String, Value>) -> Result<(), AppError> {
    for (field, validator) in &schema.fields {
        let value = data.get(field);
        // A server-filled vector may be absent: embed-on-write supplies it,
        // and with no provider configured the document is simply left out of
        // the vector index rather than rejected.
        if is_server_filled(validator) && matches!(value, None | Some(Value::Null)) {
            continue;
        }
        validate_value(field, validator, value)
            .map_err(|msg| AppError::BadRequest(format!("validation failed: {}", msg)))?;
    }
    Ok(())
}

fn validate_value(path: &str, validator: &Value, value: Option<&Value>) -> Result<(), String> {
    let vtype = validator
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{}: malformed validator", path))?;

    // Optional accepts absent or null before unwrapping to inner.
    if vtype == "optional" {
        return match value {
            None | Some(Value::Null) => Ok(()),
            Some(_) => {
                let inner = validator
                    .get("inner")
                    .ok_or_else(|| format!("{}: malformed optional validator", path))?;
                validate_value(path, inner, value)
            }
        };
    }

    let value = value.ok_or_else(|| format!("{}: missing required field", path))?;

    match vtype {
        "string" | "id" => value
            .is_string()
            .then_some(())
            .ok_or_else(|| format!("{}: expected string", path)),
        "number" => value
            .is_number()
            .then_some(())
            .ok_or_else(|| format!("{}: expected number", path)),
        "boolean" => value
            .is_boolean()
            .then_some(())
            .ok_or_else(|| format!("{}: expected boolean", path)),
        "literal" => {
            let expected = validator
                .get("value")
                .ok_or_else(|| format!("{}: malformed literal validator", path))?;
            (value == expected)
                .then_some(())
                .ok_or_else(|| format!("{}: expected literal {}", path, expected))
        }
        // Extra keys (`from`, `model`) are tolerated: they configure
        // embed-on-write, which does not affect what a document may hold.
        "vector" => {
            let dims = validator
                .get("dims")
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("{}: malformed vector validator", path))?;
            let items = value
                .as_array()
                .ok_or_else(|| format!("{}: expected array of {} numbers", path, dims))?;
            if items.len() as u64 != dims {
                return Err(format!(
                    "{}: expected {} dimensions, got {}",
                    path,
                    dims,
                    items.len()
                ));
            }
            for (i, item) in items.iter().enumerate() {
                // Checked as f32 because that is how the component is stored:
                // anything validation accepts must survive the round trip.
                let component = item
                    .as_f64()
                    .ok_or_else(|| format!("{}[{}]: expected number", path, i))?
                    as f32;
                if !component.is_finite() {
                    return Err(format!("{}[{}]: expected a finite 32-bit float", path, i));
                }
            }
            Ok(())
        }
        "array" => {
            let items = validator
                .get("items")
                .ok_or_else(|| format!("{}: malformed array validator", path))?;
            let arr = value
                .as_array()
                .ok_or_else(|| format!("{}: expected array", path))?;
            for (i, item) in arr.iter().enumerate() {
                validate_value(&format!("{}[{}]", path, i), items, Some(item))?;
            }
            Ok(())
        }
        "object" => {
            let props = validator
                .get("properties")
                .and_then(Value::as_object)
                .ok_or_else(|| format!("{}: malformed object validator", path))?;
            let obj = value
                .as_object()
                .ok_or_else(|| format!("{}: expected object", path))?;
            for (k, prop) in props {
                validate_value(&format!("{}.{}", path, k), prop, obj.get(k))?;
            }
            Ok(())
        }
        "union" => {
            let variants = validator
                .get("variants")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("{}: malformed union validator", path))?;
            for variant in variants {
                if validate_value(path, variant, Some(value)).is_ok() {
                    return Ok(());
                }
            }
            Err(format!("{}: no union variant matched", path))
        }
        other => Err(format!("{}: unknown validator type {:?}", path, other)),
    }
}
