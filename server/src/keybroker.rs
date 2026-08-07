//! Instance-derived API keys, modelled on Convex's keybroker.
//!
//! A key is `{instance_name}|{base64url(nonce || ciphertext)}`. The ciphertext
//! seals rmp-encoded [`KeyClaims`] with XChaCha20-Poly1305 under the 32-byte
//! instance secret, with the instance name as associated data.
//!
//! Verification is entirely local — decrypt and check the name — so there is no
//! key store to replicate across regions, and rotating `INSTANCE_SECRET`
//! revokes every key derived from the old one at once. Self-hosters mint keys
//! with `brickbed-server keygen`; no control plane is involved.
//!
//! XChaCha20 (24-byte nonce) rather than ChaCha20 (12-byte) because nonces are
//! random per key and nothing persists a counter: 24 bytes makes collisions a
//! non-issue however many keys an instance mints.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, Generate, Key, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

/// Instance secrets are 32 bytes, hex-encoded in `INSTANCE_SECRET`.
pub const SECRET_BYTES: usize = 32;
const NONCE_BYTES: usize = 24;
/// Separates the instance name from the sealed payload. Instance names may not
/// contain it, so `split_once` recovers the name exactly.
const SEPARATOR: char = '|';

pub const DEFAULT_INSTANCE_NAME: &str = "brickbed";

#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    #[error("instance secret must be 32 bytes of hex (64 characters)")]
    InvalidSecret,

    #[error("instance name must be non-empty and must not contain '|'")]
    InvalidInstanceName,

    #[error("malformed key")]
    Malformed,

    #[error("key was issued for a different instance")]
    WrongInstance,

    #[error("key failed authentication")]
    Unauthentic,

    #[error("claims encode error: {0}")]
    Encode(String),
}

/// What a key grants. `project` of `*` grants every project, matching the
/// static `API_KEYS` convention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyClaims {
    pub project: String,
    pub issued_ms: u64,
}

/// Mints and verifies instance-derived keys. Cloning is cheap and shares no
/// mutable state.
#[derive(Clone)]
pub struct KeyBroker {
    instance_name: String,
    cipher: XChaCha20Poly1305,
}

/// Redacted so a `{:?}` of `Config` or `AppState` can never print the secret.
impl std::fmt::Debug for KeyBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyBroker")
            .field("instance_name", &self.instance_name)
            .field("secret", &"<redacted>")
            .finish()
    }
}

impl KeyBroker {
    pub fn new(instance_name: &str, secret: &[u8]) -> Result<Self, KeyError> {
        if instance_name.is_empty() || instance_name.contains(SEPARATOR) {
            return Err(KeyError::InvalidInstanceName);
        }
        if secret.len() != SECRET_BYTES {
            return Err(KeyError::InvalidSecret);
        }
        let cipher =
            XChaCha20Poly1305::new_from_slice(secret).map_err(|_| KeyError::InvalidSecret)?;
        Ok(Self {
            instance_name: instance_name.to_string(),
            cipher,
        })
    }

    /// Build from the hex form stored in `INSTANCE_SECRET`.
    pub fn from_hex(instance_name: &str, secret_hex: &str) -> Result<Self, KeyError> {
        let secret = decode_hex(secret_hex.trim()).ok_or(KeyError::InvalidSecret)?;
        Self::new(instance_name, &secret)
    }

    pub fn instance_name(&self) -> &str {
        &self.instance_name
    }

    pub fn issue(&self, project: &str) -> Result<String, KeyError> {
        self.issue_at(project, now_ms())
    }

    /// Issue with an explicit timestamp. Exists so tests do not depend on the
    /// clock.
    pub fn issue_at(&self, project: &str, issued_ms: u64) -> Result<String, KeyError> {
        let claims = KeyClaims {
            project: project.to_string(),
            issued_ms,
        };
        let plaintext = rmp_serde::to_vec(&claims).map_err(|e| KeyError::Encode(e.to_string()))?;

        let nonce = XNonce::generate();
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &plaintext,
                    aad: self.instance_name.as_bytes(),
                },
            )
            .map_err(|_| KeyError::Unauthentic)?;

        let mut sealed = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);

        Ok(format!(
            "{}{}{}",
            self.instance_name,
            SEPARATOR,
            URL_SAFE_NO_PAD.encode(sealed)
        ))
    }

    /// Decrypt and authenticate a key. Any failure — wrong instance, tampered
    /// bytes, foreign secret — is an error; callers map all of them to 401.
    pub fn verify(&self, key: &str) -> Result<KeyClaims, KeyError> {
        let (instance, payload) = key.split_once(SEPARATOR).ok_or(KeyError::Malformed)?;
        if instance != self.instance_name {
            return Err(KeyError::WrongInstance);
        }

        let sealed = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| KeyError::Malformed)?;
        if sealed.len() <= NONCE_BYTES {
            return Err(KeyError::Malformed);
        }

        let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_BYTES);
        let mut nonce = [0u8; NONCE_BYTES];
        nonce.copy_from_slice(nonce_bytes);

        let plaintext = self
            .cipher
            .decrypt(
                &XNonce::from(nonce),
                Payload {
                    msg: ciphertext,
                    aad: self.instance_name.as_bytes(),
                },
            )
            .map_err(|_| KeyError::Unauthentic)?;

        rmp_serde::from_slice(&plaintext).map_err(|_| KeyError::Unauthentic)
    }
}

/// A fresh random instance secret, hex-encoded for `INSTANCE_SECRET`.
pub fn generate_secret_hex() -> String {
    encode_hex(&Key::<XChaCha20Poly1305>::generate())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// ---- keygen CLI ----

#[derive(Debug, PartialEq, Eq)]
pub enum KeygenCommand {
    GenerateSecret,
    IssueKey { project: String },
}

pub const KEYGEN_USAGE: &str = "usage:
  brickbed-server keygen --project <name|*>   mint a key (needs INSTANCE_SECRET)
  brickbed-server keygen --generate-secret    print a fresh INSTANCE_SECRET";

/// Parse `keygen` arguments. Accepts `--project x` and `--project=x`.
pub fn parse_keygen_args(args: &[String]) -> Result<KeygenCommand, String> {
    let mut project: Option<String> = None;
    let mut generate_secret = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--generate-secret" => generate_secret = true,
            "--project" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--project needs a value".to_string())?;
                project = Some(value.clone());
            }
            other => match other.strip_prefix("--project=") {
                Some(value) => project = Some(value.to_string()),
                None => return Err(format!("unknown argument {:?}", other)),
            },
        }
    }

    match (generate_secret, project) {
        (true, None) => Ok(KeygenCommand::GenerateSecret),
        (true, Some(_)) => Err("--generate-secret takes no --project".to_string()),
        (false, Some(project)) if !project.is_empty() => Ok(KeygenCommand::IssueKey { project }),
        (false, Some(_)) => Err("--project needs a non-empty value".to_string()),
        (false, None) => Err("--project is required".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET_A: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    const SECRET_B: &str = "ffeeddccbbaa99887766554433221100ffeeddccbbaa99887766554433221100";

    fn broker(name: &str, secret: &str) -> KeyBroker {
        KeyBroker::from_hex(name, secret).unwrap()
    }

    #[test]
    fn issue_verify_roundtrip() {
        let broker = broker("brickbed", SECRET_A);
        let key = broker.issue_at("demo", 1_700_000_000_000).unwrap();

        assert!(key.starts_with("brickbed|"));
        let claims = broker.verify(&key).unwrap();
        assert_eq!(claims.project, "demo");
        assert_eq!(claims.issued_ms, 1_700_000_000_000);
    }

    #[test]
    fn wildcard_project_survives_the_roundtrip() {
        let broker = broker("brickbed", SECRET_A);
        let key = broker.issue("*").unwrap();
        assert_eq!(broker.verify(&key).unwrap().project, "*");
    }

    #[test]
    fn each_key_is_unique_even_for_one_project() {
        let broker = broker("brickbed", SECRET_A);
        let first = broker.issue_at("demo", 42).unwrap();
        let second = broker.issue_at("demo", 42).unwrap();
        assert_ne!(first, second, "nonce must be random per key");
        assert_eq!(
            broker.verify(&first).unwrap(),
            broker.verify(&second).unwrap()
        );
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let broker = broker("brickbed", SECRET_A);
        let key = broker.issue("demo").unwrap();
        let (name, payload) = key.split_once('|').unwrap();

        // Flip one base64 character of the sealed payload.
        let mut chars: Vec<char> = payload.chars().collect();
        let last = chars.len() - 1;
        chars[last] = if chars[last] == 'A' { 'B' } else { 'A' };
        let tampered = format!("{}|{}", name, chars.into_iter().collect::<String>());

        assert!(matches!(
            broker.verify(&tampered),
            Err(KeyError::Unauthentic) | Err(KeyError::Malformed)
        ));
    }

    #[test]
    fn key_from_another_secret_is_rejected() {
        let issuer = broker("brickbed", SECRET_A);
        let verifier = broker("brickbed", SECRET_B);
        let key = issuer.issue("demo").unwrap();
        assert!(matches!(verifier.verify(&key), Err(KeyError::Unauthentic)));
    }

    #[test]
    fn key_from_another_instance_is_rejected() {
        let issuer = broker("other-instance", SECRET_A);
        let verifier = broker("brickbed", SECRET_A);
        let key = issuer.issue("demo").unwrap();
        assert!(matches!(
            verifier.verify(&key),
            Err(KeyError::WrongInstance)
        ));

        // Even relabelled to the right instance, the name is authenticated data.
        let relabelled = key.replace("other-instance|", "brickbed|");
        assert!(matches!(
            verifier.verify(&relabelled),
            Err(KeyError::Unauthentic)
        ));
    }

    #[test]
    fn malformed_keys_are_rejected() {
        let broker = broker("brickbed", SECRET_A);
        for key in [
            "",
            "no-separator",
            "brickbed|",
            "brickbed|!!!not-base64!!!",
            // Valid base64 but too short to hold a nonce.
            "brickbed|AAAA",
        ] {
            assert!(broker.verify(key).is_err(), "accepted {:?}", key);
        }
    }

    #[test]
    fn secrets_and_instance_names_are_validated() {
        assert!(matches!(
            KeyBroker::from_hex("brickbed", "abcd"),
            Err(KeyError::InvalidSecret)
        ));
        assert!(matches!(
            KeyBroker::from_hex(
                "brickbed",
                "zz112233445566778899aabbccddeeff00112233445566778899aabbccddeeff"
            ),
            Err(KeyError::InvalidSecret)
        ));
        assert!(matches!(
            KeyBroker::from_hex("", SECRET_A),
            Err(KeyError::InvalidInstanceName)
        ));
        assert!(matches!(
            KeyBroker::from_hex("has|pipe", SECRET_A),
            Err(KeyError::InvalidInstanceName)
        ));
        // Surrounding whitespace in the env var is tolerated.
        assert!(KeyBroker::from_hex("brickbed", &format!("  {}\n", SECRET_A)).is_ok());
    }

    #[test]
    fn generated_secrets_are_usable_and_distinct() {
        let secret = generate_secret_hex();
        assert_eq!(secret.len(), SECRET_BYTES * 2);
        assert_ne!(secret, generate_secret_hex());

        let broker = KeyBroker::from_hex("brickbed", &secret).unwrap();
        let key = broker.issue("demo").unwrap();
        assert_eq!(broker.verify(&key).unwrap().project, "demo");
    }

    #[test]
    fn keygen_args_parse() {
        let args =
            |parts: &[&str]| -> Vec<String> { parts.iter().map(|s| s.to_string()).collect() };

        assert_eq!(
            parse_keygen_args(&args(&["--project", "demo"])).unwrap(),
            KeygenCommand::IssueKey {
                project: "demo".to_string()
            }
        );
        assert_eq!(
            parse_keygen_args(&args(&["--project=*"])).unwrap(),
            KeygenCommand::IssueKey {
                project: "*".to_string()
            }
        );
        assert_eq!(
            parse_keygen_args(&args(&["--generate-secret"])).unwrap(),
            KeygenCommand::GenerateSecret
        );

        for bad in [
            vec![],
            args(&["--project"]),
            args(&["--project", ""]),
            args(&["--nope"]),
            args(&["--generate-secret", "--project", "demo"]),
        ] {
            assert!(parse_keygen_args(&bad).is_err(), "accepted {:?}", bad);
        }
    }
}
