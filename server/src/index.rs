//! Index entry key encoding.
//!
//! Entry key layout:
//!   `_idx:{project}:{collection}:{index}:` enc(v1) 0x00 enc(v2) 0x00 ... 0x00 {id}
//!
//! Encoded values never contain 0x00 (escaped), so a scan on
//! `prefix + enc(v1) 0x00` matches exactly complete values, and shorter
//! param sets prefix-match composite indexes. The entry value stores the
//! document id.

use serde_json::{Map, Value};

use crate::schema::IndexDef;

/// Separator between encoded values (and between an encoded term and a doc id
/// in `fts.rs`, which shares this escape scheme).
pub const SEP: u8 = 0x00;

/// Escape so encoded bytes never contain 0x00: 0x00 -> 0x01 0x02, 0x01 -> 0x01 0x03.
pub fn escape_into(bytes: &[u8], out: &mut Vec<u8>) {
    for &b in bytes {
        match b {
            0x00 => out.extend_from_slice(&[0x01, 0x02]),
            0x01 => out.extend_from_slice(&[0x01, 0x03]),
            b => out.push(b),
        }
    }
}

/// Deterministic, order-preserving-per-type encoding with a leading type tag.
/// Missing/null fields encode as a bare tag so sparse docs stay indexed.
fn encode_value(value: Option<&Value>, out: &mut Vec<u8>) {
    match value {
        None | Some(Value::Null) => out.push(b'n'),
        Some(Value::Bool(b)) => {
            out.push(b'b');
            out.push(if *b { b'1' } else { b'0' });
        }
        Some(Value::Number(n)) => {
            // Sortable f64 encoding: flip sign bit for positives, all bits for
            // negatives, hex-encode (never contains 0x00).
            let f = n.as_f64().unwrap_or(f64::NAN);
            let bits = f.to_bits();
            let sortable = if f.is_sign_negative() {
                !bits
            } else {
                bits ^ (1u64 << 63)
            };
            out.push(b'f');
            out.extend_from_slice(format!("{:016x}", sortable).as_bytes());
        }
        Some(Value::String(s)) => {
            out.push(b's');
            escape_into(s.as_bytes(), out);
        }
        // Arrays/objects as index values: encode via canonical JSON. Equality
        // only; not meaningfully ordered.
        Some(other) => {
            out.push(b'j');
            escape_into(other.to_string().as_bytes(), out);
        }
    }
}

pub fn index_base(project: &str, collection: &str, index_name: &str) -> Vec<u8> {
    format!("_idx:{}:{}:{}:", project, collection, index_name).into_bytes()
}

/// Prefix for all index entries of a project (used for backfill cleanup).
pub fn project_index_prefix(project: &str) -> Vec<u8> {
    format!("_idx:{}:", project).into_bytes()
}

/// Full entry key for a document under an index.
pub fn entry_key(
    project: &str,
    collection: &str,
    index: &IndexDef,
    data: &Map<String, Value>,
    id: &str,
) -> Vec<u8> {
    let mut key = index_base(project, collection, &index.name);
    for field in &index.fields {
        encode_value(data.get(field), &mut key);
        key.push(SEP);
    }
    key.extend_from_slice(id.as_bytes());
    key
}

/// Fields an equality query binds: a non-empty prefix of the index fields,
/// with no params left over.
pub fn bound_fields<'a>(
    index: &'a IndexDef,
    params: &Map<String, Value>,
) -> Result<Vec<&'a String>, String> {
    let bound: Vec<&String> = index
        .fields
        .iter()
        .take_while(|f| params.contains_key(*f))
        .collect();

    if bound.is_empty() {
        return Err(format!(
            "params must include the first field of index ({:?})",
            index.fields
        ));
    }
    if params.len() != bound.len() {
        return Err(format!(
            "params must be a prefix of index fields {:?}",
            index.fields
        ));
    }
    Ok(bound)
}

/// Whether a document satisfies an equality predicate, evaluated on the
/// document itself rather than by scanning the index. Mirrors `encode_value`
/// so a post-filter keeps the same semantics as the equivalent `_query`: an
/// absent field and an explicit null are the same thing, and numbers compare
/// by value so `1` matches `1.0`.
pub fn matches_params(
    fields: &[&String],
    params: &Map<String, Value>,
    data: &Map<String, Value>,
) -> bool {
    fields
        .iter()
        .all(|field| value_eq(data.get(*field), params.get(*field)))
}

/// Equality is defined as "encodes to the same index bytes" — by construction
/// the post-filter can never diverge from the scan, for any value shape, even
/// if `encode_value` changes.
fn value_eq(a: Option<&Value>, b: Option<&Value>) -> bool {
    let mut ea = Vec::new();
    encode_value(a, &mut ea);
    let mut eb = Vec::new();
    encode_value(b, &mut eb);
    ea == eb
}

/// Scan prefix for a query binding the first `params.len()` fields of the
/// index. Params must cover a non-empty prefix of `index.fields`.
pub fn query_prefix(
    project: &str,
    collection: &str,
    index: &IndexDef,
    params: &Map<String, Value>,
) -> Result<Vec<u8>, String> {
    let bound = bound_fields(index, params)?;

    let mut prefix = index_base(project, collection, &index.name);
    for field in bound {
        encode_value(params.get(field), &mut prefix);
        prefix.push(SEP);
    }
    Ok(prefix)
}

/// Opaque query cursor: hex of the last-returned index entry key.
pub fn encode_cursor(key: &[u8]) -> String {
    key.iter().map(|b| format!("{:02x}", b)).collect()
}

pub fn decode_cursor(cursor: &str) -> Option<Vec<u8>> {
    if !cursor.len().is_multiple_of(2) {
        return None;
    }
    (0..cursor.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cursor[i..i + 2], 16).ok())
        .collect()
}

/// Exclusive upper bound for a prefix scan: increment the last byte,
/// carrying over trailing 0xFF bytes. Our prefixes end in 0x00 or `:` so
/// the carry never exhausts the key.
pub fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last == 0xFF {
            end.pop();
        } else {
            *last += 1;
            break;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A post-filter must accept exactly the documents whose index entry the
    /// equivalent `_query` would scan, so the two encodings are compared
    /// directly rather than trusting them to agree by inspection.
    fn agrees_with_index(field: &str, doc_value: &Value, param: &Value) -> bool {
        let index = IndexDef {
            name: "idx".to_string(),
            fields: vec![field.to_string()],
        };
        let data = json!({ field: doc_value });
        let data = data.as_object().unwrap();
        let params = json!({ field: param });
        let params = params.as_object().unwrap();

        let entry = entry_key("proj", "posts", &index, data, "01ABC");
        let prefix = query_prefix("proj", "posts", &index, params).unwrap();
        let scanned = entry.starts_with(&prefix);

        let bound = bound_fields(&index, params).unwrap();
        let filtered = matches_params(&bound, params, data);

        assert_eq!(
            scanned, filtered,
            "index scan and post-filter disagree on {} == {}",
            doc_value, param
        );
        filtered
    }

    #[test]
    fn post_filter_matches_the_index_scan() {
        assert!(agrees_with_index(
            "status",
            &json!("published"),
            &json!("published")
        ));
        assert!(!agrees_with_index(
            "status",
            &json!("draft"),
            &json!("published")
        ));

        // Integers and floats of the same value share an encoding.
        assert!(agrees_with_index("n", &json!(1), &json!(1.0)));
        assert!(!agrees_with_index("n", &json!(1), &json!(2)));

        // Signed zero is separated by the encoding's sign-bit branch.
        assert!(!agrees_with_index("n", &json!(-0.0), &json!(0.0)));
        assert!(agrees_with_index("n", &json!(-0.0), &json!(-0.0)));

        assert!(agrees_with_index("flag", &json!(true), &json!(true)));
        assert!(!agrees_with_index("flag", &json!(true), &json!(false)));
        assert!(agrees_with_index(
            "tags",
            &json!(["a", "b"]),
            &json!(["a", "b"])
        ));
        assert!(!agrees_with_index("tags", &json!(["a"]), &json!(["b"])));

        // Compound values encode (and therefore compare) textually: numeric
        // equivalence and signed-zero folding do NOT apply inside them.
        assert!(!agrees_with_index("tags", &json!([1]), &json!([1.0])));
        assert!(!agrees_with_index("tags", &json!([-0.0]), &json!([0.0])));

        // Object keys are canonically ordered (serde_json BTreeMap). This
        // assertion fails if a dependency ever enables preserve_order, which
        // would silently split equal objects into distinct index entries.
        assert!(agrees_with_index(
            "meta",
            &json!({"a": 1, "b": 2}),
            &json!({"b": 2, "a": 1})
        ));
    }

    #[test]
    fn absent_and_null_are_the_same_value() {
        assert!(agrees_with_index("missing", &json!(null), &json!(null)));

        // An absent field encodes as null, so a null param matches it.
        let index = IndexDef {
            name: "idx".to_string(),
            fields: vec!["missing".to_string()],
        };
        let empty = serde_json::Map::new();
        let params = json!({"missing": null});
        let params = params.as_object().unwrap();
        let bound = bound_fields(&index, params).unwrap();
        assert!(matches_params(&bound, params, &empty));

        let entry = entry_key("proj", "posts", &index, &empty, "01ABC");
        let prefix = query_prefix("proj", "posts", &index, params).unwrap();
        assert!(entry.starts_with(&prefix));
    }

    #[test]
    fn composite_params_must_bind_a_prefix() {
        let index = IndexDef {
            name: "by_status".to_string(),
            fields: vec!["status".to_string(), "publishedAt".to_string()],
        };

        let first = json!({"status": "published"});
        assert_eq!(
            bound_fields(&index, first.as_object().unwrap())
                .unwrap()
                .len(),
            1
        );

        let both = json!({"status": "published", "publishedAt": 10});
        assert_eq!(
            bound_fields(&index, both.as_object().unwrap())
                .unwrap()
                .len(),
            2
        );

        // Skipping the leading field, or passing one the index does not hold.
        let second_only = json!({"publishedAt": 10});
        assert!(bound_fields(&index, second_only.as_object().unwrap()).is_err());
        let extra = json!({"status": "published", "other": 1});
        assert!(bound_fields(&index, extra.as_object().unwrap()).is_err());
    }
}
