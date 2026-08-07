//! Declarative per-collection access rules, including the wire format and
//! evaluation table.
//!
//! Two-step by construction: [`collection`] answers "may this principal attempt
//! the operation at all", and returns [`Access::OwnerScoped`] when a per-document
//! check is still owed. The caller cannot get a bare "yes" out of an owner rule,
//! which is what stops a handler from forgetting the second half.

use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::jwt::{Identity, Principal};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RuleError {
    /// No credential, or an anonymous caller where identity is required — 401.
    #[error("authentication required")]
    Unauthenticated,

    /// Authenticated, but not permitted — 403.
    #[error("forbidden")]
    Forbidden,
}

/// Which side of the rules a request is asking for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Read,
    Create,
    Update,
    Delete,
}

/// What an owner rule compares the document field against.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OwnerMatch {
    /// The `sub` claim. Default: it is what applications already store.
    #[default]
    Subject,
    /// `{issuer}|{subject}` — the safe choice when two providers are configured.
    TokenIdentifier,
    Email,
}

impl OwnerMatch {
    fn value<'a>(&self, identity: &'a Identity) -> Option<&'a str> {
        match self {
            OwnerMatch::Subject => Some(&identity.subject),
            OwnerMatch::TokenIdentifier => Some(&identity.token_identifier),
            OwnerMatch::Email => identity.email.as_deref(),
        }
    }
}

/// One side of a collection's rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rule {
    /// No credential required at all.
    Public,
    /// Any valid end-user token.
    Authenticated,
    /// Valid token whose identity matches `field` on the document.
    Owner { field: String, match_on: OwnerMatch },
}

/// The `write` side is a union so per-operation rules can be added without a
/// wire break. Both forms are evaluated — accepting configuration we then
/// ignored would be worse than rejecting it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum WriteRule {
    All(Rule),
    PerOp {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        create: Option<Rule>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        update: Option<Rule>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delete: Option<Rule>,
    },
}

impl WriteRule {
    pub(crate) fn for_op(&self, op: Op) -> Option<&Rule> {
        match self {
            WriteRule::All(rule) => Some(rule),
            WriteRule::PerOp {
                create,
                update,
                delete,
            } => match op {
                Op::Create => create.as_ref(),
                Op::Update => update.as_ref(),
                Op::Delete => delete.as_ref(),
                Op::Read => None,
            },
        }
    }
}

/// A collection's `rules` block. An omitted side denies end users outright.
/// `deny_unknown_fields` for the same reason Rule/WriteRule hand-parse: a
/// typo'd policy key must be a push error, never a silently different policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionRules {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read: Option<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<WriteRule>,
}

impl CollectionRules {
    /// Every field an owner rule matches on, across both sides.
    pub fn owner_fields(&self) -> Vec<&str> {
        let sides = [
            self.read.as_ref(),
            self.write.as_ref().and_then(|w| w.for_op(Op::Create)),
            self.write.as_ref().and_then(|w| w.for_op(Op::Update)),
            self.write.as_ref().and_then(|w| w.for_op(Op::Delete)),
        ];
        sides
            .into_iter()
            .flatten()
            .filter_map(|rule| match rule {
                Rule::Owner { field, .. } => Some(field.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Does any side need a verified end user? `"public"` alone does not, so a
    /// public-read collection works with no auth providers configured.
    pub fn names_an_identity_rule(&self) -> bool {
        let needs = |rule: Option<&Rule>| !matches!(rule, None | Some(Rule::Public));
        needs(self.read.as_ref())
            || [Op::Create, Op::Update, Op::Delete]
                .into_iter()
                .any(|op| needs(self.write.as_ref().and_then(|w| w.for_op(op))))
    }

    fn for_op(&self, op: Op) -> Option<&Rule> {
        match op {
            Op::Read => self.read.as_ref(),
            _ => self.write.as_ref().and_then(|w| w.for_op(op)),
        }
    }
}

// ---- wire encoding ----
//
// `"public"` | `"authenticated"` | `{"owner": "field"}` |
// `{"owner": "field", "match": "tokenIdentifier"}`

/// Parsed by hand rather than with `#[serde(untagged)]`: untagged silently
/// ignores unknown keys, so a typo like `{"owner": "f", "matsh": "email"}` would
/// quietly become a *subject* match. A rule that does not mean what its author
/// wrote is exactly the bug class this DSL exists to avoid.
fn rule_from_value(value: &Value) -> Result<Rule, String> {
    if let Some(word) = value.as_str() {
        return match word {
            "public" => Ok(Rule::Public),
            "authenticated" => Ok(Rule::Authenticated),
            other => Err(format!(
                "unknown rule {:?}: expected \"public\", \"authenticated\", or {{\"owner\": \"<field>\"}}",
                other
            )),
        };
    }

    let object = value
        .as_object()
        .ok_or_else(|| format!("expected a rule, found {}", value))?;

    for key in object.keys() {
        if key != "owner" && key != "match" {
            return Err(format!("unknown key {:?} in owner rule", key));
        }
    }

    let field = match object.get("owner") {
        Some(Value::String(field)) if !field.is_empty() => field.clone(),
        Some(Value::String(_)) => return Err("owner rule needs a field name".to_string()),
        Some(other) => return Err(format!("owner must be a field name, found {}", other)),
        None => return Err("expected \"owner\" in rule object".to_string()),
    };

    // An explicit null is a mistake worth reporting, not a silent default.
    let match_on = match object.get("match") {
        None => OwnerMatch::default(),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|_| format!("unknown owner match {}", value))?,
    };

    Ok(Rule::Owner { field, match_on })
}

impl<'de> Deserialize<'de> for Rule {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        rule_from_value(&value).map_err(de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for WriteRule {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;

        // Anything that is a rule in its own right is the whole-write form; the
        // per-operation form is an object keyed only by operation names.
        let is_per_op = value
            .as_object()
            .is_some_and(|object| !object.contains_key("owner"));
        if !is_per_op {
            return rule_from_value(&value)
                .map(WriteRule::All)
                .map_err(de::Error::custom);
        }

        let object = value.as_object().expect("checked above");
        let mut rules: [Option<Rule>; 3] = [None, None, None];
        for (key, raw) in object {
            let slot = match key.as_str() {
                "create" => 0,
                "update" => 1,
                "delete" => 2,
                other => {
                    return Err(de::Error::custom(format!(
                        "unknown key {:?} in write rule: expected \"create\", \"update\", \"delete\", or a rule",
                        other
                    )))
                }
            };
            rules[slot] = Some(rule_from_value(raw).map_err(de::Error::custom)?);
        }

        let [create, update, delete] = rules;
        if create.is_none() && update.is_none() && delete.is_none() {
            return Err(de::Error::custom("write rule is empty"));
        }
        Ok(WriteRule::PerOp {
            create,
            update,
            delete,
        })
    }
}

impl Serialize for Rule {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Rule::Public => serializer.serialize_str("public"),
            Rule::Authenticated => serializer.serialize_str("authenticated"),
            Rule::Owner { field, match_on } => {
                use serde::ser::SerializeMap;
                let mut map = serializer.serialize_map(None)?;
                map.serialize_entry("owner", field)?;
                if *match_on != OwnerMatch::default() {
                    map.serialize_entry("match", match_on)?;
                }
                map.end()
            }
        }
    }
}

// ---- evaluation ----

/// Outcome of the collection-level check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    /// Nothing further to check.
    Granted,
    /// Permitted only for documents this identity owns; the caller still owes a
    /// [`OwnerCheck::permits`] per document, or [`retain`] over a result set.
    OwnerScoped(OwnerCheck),
}

impl Access {
    /// Enforce an owner-scoped access against the documents an operation
    /// touches. Unconditional access passes without looking at them.
    ///
    /// `existing` is the stored document (reads, updates, deletes); `next` is
    /// the document the request would store (creates, updates). An update must
    /// satisfy the rule on **both**: checking only `existing` lets a user hand a
    /// document away, checking only `next` lets them take one.
    pub fn permit(
        &self,
        op: Op,
        existing: Option<&Map<String, Value>>,
        next: Option<&Map<String, Value>>,
    ) -> Result<(), RuleError> {
        let Access::OwnerScoped(check) = self else {
            return Ok(());
        };

        // Every side the operation needs must actually be supplied. A `None`
        // where a document was required means ownership was never demonstrated,
        // so it denies — a caller that forgets to load the document gets a 403,
        // not a silent pass.
        let required: [Option<&Map<String, Value>>; 2] = match op {
            Op::Read | Op::Delete => [Some(existing.ok_or(RuleError::Forbidden)?), None],
            Op::Create => [Some(next.ok_or(RuleError::Forbidden)?), None],
            Op::Update => [
                Some(existing.ok_or(RuleError::Forbidden)?),
                Some(next.ok_or(RuleError::Forbidden)?),
            ],
        };

        for side in required.into_iter().flatten() {
            if !check.permits(side) {
                return Err(RuleError::Forbidden);
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerCheck {
    pub field: String,
    pub match_on: OwnerMatch,
    identity: Identity,
}

impl OwnerCheck {
    /// A missing field, a null, or a non-string value denies. Absence is never
    /// treated as a match.
    pub fn permits(&self, data: &Map<String, Value>) -> bool {
        let Some(expected) = self.match_on.value(&self.identity) else {
            return false;
        };
        if expected.is_empty() {
            return false;
        }
        data.get(&self.field)
            .and_then(Value::as_str)
            .is_some_and(|actual| actual == expected)
    }
}

/// May this principal attempt `op` on this collection?
///
/// API keys bypass rules entirely: they are server-side credentials that
/// already carry the whole project.
pub fn collection(
    principal: &Principal,
    rules: Option<&CollectionRules>,
    op: Op,
) -> Result<Access, RuleError> {
    if principal.is_api_key() {
        return Ok(Access::Granted);
    }

    // No rules at all means the collection was never opened to end users, which
    // is the untouched-behaviour guarantee for every existing project.
    let Some(rule) = rules.and_then(|r| r.for_op(op)) else {
        return Err(deny(principal));
    };

    match rule {
        Rule::Public => Ok(Access::Granted),
        Rule::Authenticated => match principal.identity() {
            Some(_) => Ok(Access::Granted),
            None => Err(deny(principal)),
        },
        Rule::Owner { field, match_on } => match principal.identity() {
            Some(identity) => Ok(Access::OwnerScoped(OwnerCheck {
                field: field.clone(),
                match_on: *match_on,
                identity: identity.clone(),
            })),
            None => Err(deny(principal)),
        },
    }
}

/// Anonymous callers get 401 (a credential might help); authenticated ones get
/// 403 (it would not).
fn deny(principal: &Principal) -> RuleError {
    match principal {
        Principal::Anonymous => RuleError::Unauthenticated,
        _ => RuleError::Forbidden,
    }
}

/// Full check for an operation on a specific document.
///
/// `existing` is the stored document (reads, updates, deletes); `next` is the
/// document the request would store (creates, updates). An update must satisfy
/// the rule on **both**: checking only `existing` lets a user hand a document
/// away, checking only `next` lets them take one.
pub fn document(
    principal: &Principal,
    rules: Option<&CollectionRules>,
    op: Op,
    existing: Option<&Map<String, Value>>,
    next: Option<&Map<String, Value>>,
) -> Result<(), RuleError> {
    collection(principal, rules, op)?.permit(op, existing, next)
}

/// Drop results the principal may not see. Generic over the hit shape via an
/// accessor so it serves plain documents and search's scored tuples alike.
pub fn retain<T>(access: &Access, items: &mut Vec<T>, data_of: impl Fn(&T) -> &Map<String, Value>) {
    let Access::OwnerScoped(check) = access else {
        return;
    };
    items.retain(|item| check.permits(data_of(item)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity(subject: &str) -> Identity {
        Identity {
            issuer: "https://idp.example.com".to_string(),
            subject: subject.to_string(),
            token_identifier: format!("https://idp.example.com|{}", subject),
            email: Some(format!("{}@example.com", subject)),
            claims: Map::new(),
        }
    }

    fn user(subject: &str) -> Principal {
        Principal::EndUser(identity(subject))
    }

    fn api_key() -> Principal {
        Principal::ApiKey {
            project: "demo".to_string(),
        }
    }

    fn doc(owner: &str) -> Map<String, Value> {
        json!({"authorId": owner, "title": "a post"})
            .as_object()
            .unwrap()
            .clone()
    }

    fn rules(json: Value) -> CollectionRules {
        serde_json::from_value(json).unwrap()
    }

    // ---- wire format ----

    #[test]
    fn parses_every_rule_form() {
        let parsed = rules(json!({"read": "public", "write": {"owner": "authorId"}}));
        assert_eq!(parsed.read, Some(Rule::Public));
        assert_eq!(
            parsed.write,
            Some(WriteRule::All(Rule::Owner {
                field: "authorId".to_string(),
                match_on: OwnerMatch::Subject
            }))
        );

        let parsed = rules(json!({"read": "authenticated"}));
        assert_eq!(parsed.read, Some(Rule::Authenticated));
        assert_eq!(parsed.write, None);

        let parsed = rules(json!({
            "read": {"owner": "authorId", "match": "tokenIdentifier"}
        }));
        assert_eq!(
            parsed.read,
            Some(Rule::Owner {
                field: "authorId".to_string(),
                match_on: OwnerMatch::TokenIdentifier
            })
        );
    }

    #[test]
    fn parses_the_per_op_write_union() {
        let parsed = rules(json!({
            "write": {"create": "authenticated", "update": {"owner": "authorId"}}
        }));
        let write = parsed.write.as_ref().unwrap();
        assert_eq!(write.for_op(Op::Create), Some(&Rule::Authenticated));
        assert!(matches!(write.for_op(Op::Update), Some(Rule::Owner { .. })));
        // An operation the config never mentions stays denied.
        assert_eq!(write.for_op(Op::Delete), None);
    }

    #[test]
    fn rejects_unknown_and_malformed_rules() {
        for bad in [
            json!({"read": "everyone"}),
            json!({"read": "Public"}),
            json!({"read": {"owner": ""}}),
            json!({"read": {"owner": "f", "match": "nickname"}}),
            json!({"read": {"ownerr": "f"}}),
            json!({"read": 42}),
            json!({"read": ["public"]}),
            json!({"read": {"owner": 42}}),
            json!({"read": {"owner": ["f"]}}),
            // A typo'd selector must not silently degrade to a subject match.
            json!({"read": {"owner": "f", "matsh": "email"}}),
            json!({"read": {"owner": "f", "match": null}}),
            json!({"write": {"creat": "public"}}),
            json!({"write": {}}),
        ] {
            assert!(
                serde_json::from_value::<CollectionRules>(bad.clone()).is_err(),
                "accepted {}",
                bad
            );
        }
    }

    #[test]
    fn rules_round_trip_through_json() {
        for original in [
            json!({"read": "public"}),
            json!({"read": "authenticated", "write": {"owner": "authorId"}}),
            json!({"read": {"owner": "authorId", "match": "email"}}),
            json!({}),
        ] {
            let parsed: CollectionRules = serde_json::from_value(original.clone()).unwrap();
            let reserialized = serde_json::to_value(&parsed).unwrap();
            assert_eq!(reserialized, original, "round trip changed the wire form");
        }
    }

    // ---- collection-level ----

    #[test]
    fn api_keys_bypass_every_rule() {
        for op in [Op::Read, Op::Create, Op::Update, Op::Delete] {
            // Even with no rules at all, and even for owner rules.
            assert_eq!(collection(&api_key(), None, op), Ok(Access::Granted));
            let owner =
                rules(json!({"read": {"owner": "authorId"}, "write": {"owner": "authorId"}}));
            assert_eq!(
                collection(&api_key(), Some(&owner), op),
                Ok(Access::Granted)
            );
        }
    }

    #[test]
    fn absent_rules_deny_end_users() {
        for op in [Op::Read, Op::Create, Op::Update, Op::Delete] {
            assert_eq!(collection(&user("u1"), None, op), Err(RuleError::Forbidden));
            assert_eq!(
                collection(&Principal::Anonymous, None, op),
                Err(RuleError::Unauthenticated)
            );
        }
    }

    #[test]
    fn read_rules_do_not_grant_writes() {
        let read_only = rules(json!({"read": "public"}));
        assert_eq!(
            collection(&user("u1"), Some(&read_only), Op::Read),
            Ok(Access::Granted)
        );
        for op in [Op::Create, Op::Update, Op::Delete] {
            assert_eq!(
                collection(&user("u1"), Some(&read_only), op),
                Err(RuleError::Forbidden)
            );
        }
    }

    #[test]
    fn public_admits_anonymous_but_authenticated_does_not() {
        let public = rules(json!({"read": "public"}));
        assert_eq!(
            collection(&Principal::Anonymous, Some(&public), Op::Read),
            Ok(Access::Granted)
        );

        let authenticated = rules(json!({"read": "authenticated"}));
        assert_eq!(
            collection(&Principal::Anonymous, Some(&authenticated), Op::Read),
            Err(RuleError::Unauthenticated)
        );
        assert_eq!(
            collection(&user("u1"), Some(&authenticated), Op::Read),
            Ok(Access::Granted)
        );
    }

    #[test]
    fn owner_rules_never_grant_unconditionally() {
        let owner = rules(json!({"read": {"owner": "authorId"}}));
        let access = collection(&user("u1"), Some(&owner), Op::Read).unwrap();
        assert!(
            matches!(access, Access::OwnerScoped(_)),
            "owner rule must demand a per-document check"
        );
        assert_eq!(
            collection(&Principal::Anonymous, Some(&owner), Op::Read),
            Err(RuleError::Unauthenticated)
        );
    }

    // ---- document-level ----

    #[test]
    fn owner_matches_only_the_owning_identity() {
        let r = rules(json!({"read": {"owner": "authorId"}}));
        assert_eq!(
            document(&user("u1"), Some(&r), Op::Read, Some(&doc("u1")), None),
            Ok(())
        );
        assert_eq!(
            document(&user("u2"), Some(&r), Op::Read, Some(&doc("u1")), None),
            Err(RuleError::Forbidden)
        );
    }

    #[test]
    fn owner_match_selector_changes_what_is_compared() {
        let by_token = rules(json!({"read": {"owner": "authorId", "match": "tokenIdentifier"}}));
        let token_doc = doc("https://idp.example.com|u1");
        assert_eq!(
            document(
                &user("u1"),
                Some(&by_token),
                Op::Read,
                Some(&token_doc),
                None
            ),
            Ok(())
        );
        // The bare subject no longer matches under this selector.
        assert_eq!(
            document(
                &user("u1"),
                Some(&by_token),
                Op::Read,
                Some(&doc("u1")),
                None
            ),
            Err(RuleError::Forbidden)
        );

        let by_email = rules(json!({"read": {"owner": "authorId", "match": "email"}}));
        assert_eq!(
            document(
                &user("u1"),
                Some(&by_email),
                Op::Read,
                Some(&doc("u1@example.com")),
                None
            ),
            Ok(())
        );
    }

    #[test]
    fn email_match_denies_when_the_token_carries_no_email() {
        let mut anonymous_email = identity("u1");
        anonymous_email.email = None;
        let principal = Principal::EndUser(anonymous_email);
        let by_email = rules(json!({"read": {"owner": "authorId", "match": "email"}}));

        assert_eq!(
            document(
                &principal,
                Some(&by_email),
                Op::Read,
                Some(&doc("u1@example.com")),
                None
            ),
            Err(RuleError::Forbidden)
        );
    }

    #[test]
    fn missing_or_non_string_owner_field_denies() {
        let r = rules(json!({"read": {"owner": "authorId"}}));
        for data in [
            json!({"title": "no owner field at all"}),
            json!({"authorId": null}),
            json!({"authorId": 42}),
            json!({"authorId": ["u1"]}),
            json!({"authorId": {"id": "u1"}}),
            json!({"authorId": ""}),
        ] {
            let data = data.as_object().unwrap().clone();
            assert_eq!(
                document(&user("u1"), Some(&r), Op::Read, Some(&data), None),
                Err(RuleError::Forbidden),
                "accepted {:?}",
                data
            );
        }
    }

    #[test]
    fn create_checks_the_submitted_document() {
        let r = rules(json!({"write": {"owner": "authorId"}}));
        assert_eq!(
            document(&user("u1"), Some(&r), Op::Create, None, Some(&doc("u1"))),
            Ok(())
        );
        // Creating a document owned by someone else is refused, not rewritten.
        assert_eq!(
            document(&user("u1"), Some(&r), Op::Create, None, Some(&doc("u2"))),
            Err(RuleError::Forbidden)
        );
    }

    #[test]
    fn update_requires_both_sides_to_match() {
        let r = rules(json!({"write": {"owner": "authorId"}}));

        assert_eq!(
            document(
                &user("u1"),
                Some(&r),
                Op::Update,
                Some(&doc("u1")),
                Some(&doc("u1"))
            ),
            Ok(())
        );

        // Giving your document away.
        assert_eq!(
            document(
                &user("u1"),
                Some(&r),
                Op::Update,
                Some(&doc("u1")),
                Some(&doc("u2"))
            ),
            Err(RuleError::Forbidden)
        );

        // Stealing someone else's by claiming it in the update.
        assert_eq!(
            document(
                &user("u1"),
                Some(&r),
                Op::Update,
                Some(&doc("u2")),
                Some(&doc("u1"))
            ),
            Err(RuleError::Forbidden)
        );
    }

    #[test]
    fn delete_checks_the_stored_document() {
        let r = rules(json!({"write": {"owner": "authorId"}}));
        assert_eq!(
            document(&user("u1"), Some(&r), Op::Delete, Some(&doc("u1")), None),
            Ok(())
        );
        assert_eq!(
            document(&user("u1"), Some(&r), Op::Delete, Some(&doc("u2")), None),
            Err(RuleError::Forbidden)
        );
    }

    #[test]
    fn owner_rules_deny_when_a_required_document_is_missing() {
        // A handler that forgets to load the document must not thereby pass the
        // check: absence is never ownership.
        let r = rules(json!({"read": {"owner": "authorId"}, "write": {"owner": "authorId"}}));
        let cases = [
            (Op::Read, None, None),
            (Op::Delete, None, None),
            (Op::Create, None, None),
            (Op::Update, None, None),
            (Op::Update, Some(doc("u1")), None),
            (Op::Update, None, Some(doc("u1"))),
        ];
        for (op, existing, next) in cases {
            assert_eq!(
                document(&user("u1"), Some(&r), op, existing.as_ref(), next.as_ref()),
                Err(RuleError::Forbidden),
                "{:?} passed with a missing document",
                op
            );
        }
    }

    #[test]
    fn non_owner_rules_need_no_document() {
        let r = rules(json!({"read": "public", "write": "authenticated"}));
        assert_eq!(
            document(&user("u1"), Some(&r), Op::Read, None, None),
            Ok(())
        );
        assert_eq!(
            document(&user("u1"), Some(&r), Op::Create, None, None),
            Ok(())
        );
    }

    // ---- post-filter ----

    #[test]
    fn retain_drops_documents_the_caller_does_not_own() {
        let r = rules(json!({"read": {"owner": "authorId"}}));
        let access = collection(&user("u1"), Some(&r), Op::Read).unwrap();

        let mut docs = vec![doc("u1"), doc("u2"), doc("u1")];
        retain(&access, &mut docs, |d| d);
        assert_eq!(docs.len(), 2);
        assert!(docs.iter().all(|d| d["authorId"] == "u1"));
    }

    #[test]
    fn retain_works_on_scored_search_hits() {
        let r = rules(json!({"read": {"owner": "authorId"}}));
        let access = collection(&user("u1"), Some(&r), Op::Read).unwrap();

        let mut hits = vec![(doc("u2"), 9.5_f64), (doc("u1"), 4.0), (doc("u2"), 1.0)];
        retain(&access, &mut hits, |(data, _)| data);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, 4.0);
    }

    #[test]
    fn retain_is_a_noop_when_access_is_unconditional() {
        let mut docs = vec![doc("u1"), doc("u2")];
        retain(&Access::Granted, &mut docs, |d| d);
        assert_eq!(docs.len(), 2, "granted access must not filter");
    }
}
