use super::{
    Findings, Outcome, PolicyError, Replacement, RequestContext, RequestPolicy, RestorationState,
    Verdict, secrets::find_secrets,
};
use crate::{compression::decode_declared, config::GuardrailsMode};
use axum::body::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use std::collections::BTreeMap;

pub const PLACEHOLDER_PREFIX: &str = "AEGIS_SECRET_";
pub const PLACEHOLDER_SUFFIX: &str = "_END";
const PLACEHOLDER_DIGEST_BYTES: usize = 11;
pub const PLACEHOLDER_LEN: usize =
    PLACEHOLDER_PREFIX.len() + 2 * PLACEHOLDER_DIGEST_BYTES + PLACEHOLDER_SUFFIX.len();

const SCANNED_FIELDS: [&str; 5] = ["system", "messages", "tools", "instructions", "input"];
const TOOL_CREDENTIAL_FIELDS: [&str; 2] = ["headers", "authorization_token"];
const ATTACHMENT_DATA_FIELD: &str = "data";

pub struct SecretsPolicy {
    placeholder_key: [u8; 32],
    mode: GuardrailsMode,
}

impl SecretsPolicy {
    pub fn new(placeholder_key: [u8; 32], mode: GuardrailsMode) -> Self {
        Self {
            placeholder_key,
            mode,
        }
    }

    pub fn placeholder(&self, secret: &str) -> String {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.placeholder_key)
            .expect("HMAC accepts any key");
        mac.update(secret.as_bytes());
        let digest = mac.finalize().into_bytes();
        let mut placeholder = String::with_capacity(PLACEHOLDER_LEN);
        placeholder.push_str(PLACEHOLDER_PREFIX);
        for byte in &digest[..PLACEHOLDER_DIGEST_BYTES] {
            placeholder.push_str(&format!("{byte:02x}"));
        }
        placeholder.push_str(PLACEHOLDER_SUFFIX);
        placeholder
    }
}

#[derive(Default)]
struct Scan {
    detector_counts: BTreeMap<&'static str, i64>,
    replacements: BTreeMap<String, String>,
}

impl Scan {
    fn match_count(&self) -> i64 {
        self.detector_counts.values().sum()
    }

    fn metadata(&self) -> Value {
        json!({
            "detectors": self.detector_counts,
            "placeholders": self.replacements.keys().collect::<Vec<_>>(),
        })
    }
}

impl SecretsPolicy {
    fn mask_string(&self, text: &mut String, scan: &mut Scan) {
        let findings = find_secrets(text);
        if findings.is_empty() {
            return;
        }
        let mut masked = String::with_capacity(text.len());
        let mut cursor = 0;
        for finding in &findings {
            *scan
                .detector_counts
                .entry(finding.detector.name())
                .or_insert(0) += 1;
            let placeholder = self.placeholder(finding.secret);
            masked.push_str(&text[cursor..finding.start]);
            masked.push_str(&placeholder);
            scan.replacements
                .entry(placeholder)
                .or_insert_with(|| finding.secret.to_owned());
            cursor = finding.end;
        }
        masked.push_str(&text[cursor..]);
        *text = masked;
    }

    fn mask_value(&self, value: &mut Value, scan: &mut Scan, skip: &[&str]) {
        match value {
            Value::String(text) => self.mask_string(text, scan),
            Value::Array(items) => {
                for item in items {
                    self.mask_value(item, scan, skip);
                }
            }
            Value::Object(fields) => {
                let is_attachment_source = fields.contains_key("media_type")
                    || fields
                        .get("type")
                        .and_then(Value::as_str)
                        .is_some_and(|kind| kind == "base64");
                for (name, field) in fields.iter_mut() {
                    if skip.contains(&name.as_str()) {
                        continue;
                    }
                    if is_attachment_source && name == ATTACHMENT_DATA_FIELD {
                        continue;
                    }
                    self.mask_value(field, scan, skip);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    fn mask_body(&self, fields: &mut Map<String, Value>) -> Scan {
        let mut scan = Scan::default();
        for name in SCANNED_FIELDS {
            let skip: &[&str] = if name == "tools" {
                &TOOL_CREDENTIAL_FIELDS
            } else {
                &[]
            };
            if let Some(value) = fields.get_mut(name) {
                self.mask_value(value, &mut scan, skip);
            }
        }
        scan
    }
}

impl RequestPolicy for SecretsPolicy {
    fn name(&self) -> &'static str {
        "secrets"
    }

    fn version(&self) -> u32 {
        1
    }

    fn evaluate(&self, context: &RequestContext) -> Result<Verdict, PolicyError> {
        if context.body.is_empty() {
            return Ok(Verdict::allow());
        }
        let encoding = context.content_encoding.as_deref().unwrap_or("");
        let decoded = decode_declared(encoding, &context.body).ok_or_else(|| {
            PolicyError::InvalidRequest(format!(
                "request body could not be decoded as content-encoding {encoding:?}"
            ))
        })?;
        let mut document: Value = serde_json::from_slice(&decoded).map_err(|error| {
            PolicyError::InvalidRequest(format!("request body is not JSON: {error}"))
        })?;
        let Some(fields) = document.as_object_mut() else {
            return Err(PolicyError::InvalidRequest(
                "request body is not a JSON object".to_owned(),
            ));
        };
        let scan = self.mask_body(fields);
        if scan.replacements.is_empty() {
            return Ok(Verdict::allow());
        }
        let findings = Findings {
            severity: Some("high"),
            match_count: scan.match_count(),
            metadata: scan.metadata(),
        };
        let outcome = match self.mode {
            GuardrailsMode::Observe => Outcome::Allow,
            GuardrailsMode::Mask => Outcome::Transform {
                body: Bytes::from(serde_json::to_vec(&document)?),
                restore: RestorationState {
                    replacements: scan
                        .replacements
                        .into_iter()
                        .map(|(placeholder, original)| Replacement {
                            placeholder,
                            original,
                        })
                        .collect(),
                },
            },
        };
        Ok(Verdict { outcome, findings })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB_TOKEN: &str = "ghp_TESTONLYTESTONLYTESTONLYTESTONLYTEST12";
    const AWS_KEY: &str = "AKIATESTONLYTESTONLY";

    fn policy(mode: GuardrailsMode) -> SecretsPolicy {
        SecretsPolicy::new([7; 32], mode)
    }

    fn context(body: &Bytes) -> RequestContext {
        RequestContext {
            body: body.clone(),
            content_encoding: None,
        }
    }

    fn encoded_context(body: &[u8], encoding: &str) -> RequestContext {
        RequestContext {
            body: Bytes::copy_from_slice(body),
            content_encoding: Some(encoding.to_owned()),
        }
    }

    fn body_with(secret: &str) -> Bytes {
        Bytes::from(format!(
            r#"{{"model":"claude-test","system":"stay safe","messages":[{{"role":"user","content":[{{"type":"text","text":"run export TOKEN={secret} please"}}]}}],"tools":[{{"name":"bash","description":"AKIA is fine"}}],"metadata":{{"note":"{secret}"}}}}"#
        ))
    }

    #[test]
    fn placeholders_have_a_fixed_width_and_are_stable_per_secret() {
        let policy = policy(GuardrailsMode::Mask);
        let first = policy.placeholder(GITHUB_TOKEN);
        assert_eq!(first.len(), PLACEHOLDER_LEN);
        assert_eq!(PLACEHOLDER_LEN, 39);
        assert!(first.starts_with(PLACEHOLDER_PREFIX));
        assert!(first.ends_with(PLACEHOLDER_SUFFIX));
        assert_eq!(first, policy.placeholder(GITHUB_TOKEN));
        assert_ne!(first, policy.placeholder(AWS_KEY));
        assert_ne!(
            first,
            SecretsPolicy::new([8; 32], GuardrailsMode::Mask).placeholder(GITHUB_TOKEN)
        );
        assert!(find_secrets(&first).is_empty());
    }

    #[test]
    fn a_body_without_secrets_is_allowed_and_forwarded_as_the_same_bytes() {
        let body = body_with("nothing-to-see");
        let verdict = policy(GuardrailsMode::Mask)
            .evaluate(&context(&body))
            .expect("plain JSON evaluates");
        assert!(matches!(verdict.outcome, Outcome::Allow));
        assert_eq!(verdict.findings.match_count, 0);
    }

    #[test]
    fn mask_mode_replaces_secrets_in_scanned_fields_only() {
        let body = body_with(GITHUB_TOKEN);
        let verdict = policy(GuardrailsMode::Mask)
            .evaluate(&context(&body))
            .expect("JSON evaluates");
        let Outcome::Transform {
            body: masked,
            restore,
        } = verdict.outcome
        else {
            panic!("a secret in messages must transform the body");
        };
        let masked_text = std::str::from_utf8(&masked).unwrap();
        let placeholder = policy(GuardrailsMode::Mask).placeholder(GITHUB_TOKEN);
        assert_eq!(masked_text.matches(&placeholder).count(), 1);
        assert_eq!(masked_text.matches(GITHUB_TOKEN).count(), 1);
        assert!(masked_text.contains(&format!(r#""note":"{GITHUB_TOKEN}""#)));
        assert!(
            masked_text.starts_with(r#"{"model":"claude-test","system":"stay safe","messages""#)
        );
        assert_eq!(
            restore.replacements,
            [Replacement {
                placeholder,
                original: GITHUB_TOKEN.to_owned(),
            }]
        );
        assert_eq!(verdict.findings.match_count, 1);
        assert_eq!(verdict.findings.severity, Some("high"));
        assert_eq!(
            verdict.findings.metadata["detectors"],
            json!({"github_token": 1})
        );
        assert_eq!(
            verdict
                .findings
                .metadata
                .to_string()
                .matches(GITHUB_TOKEN)
                .count(),
            0
        );
    }

    #[test]
    fn observe_mode_reports_findings_but_allows_the_request() {
        let body = body_with(GITHUB_TOKEN);
        let verdict = policy(GuardrailsMode::Observe)
            .evaluate(&context(&body))
            .expect("JSON evaluates");
        assert!(matches!(verdict.outcome, Outcome::Allow));
        assert_eq!(verdict.findings.match_count, 1);
        assert_eq!(
            verdict.findings.metadata["placeholders"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn the_same_secret_twice_shares_one_placeholder_and_counts_both_matches() {
        let body = Bytes::from(format!(
            r#"{{"instructions":"{GITHUB_TOKEN}","input":[{{"role":"user","content":"again {GITHUB_TOKEN} and {AWS_KEY}"}}]}}"#
        ));
        let verdict = policy(GuardrailsMode::Mask)
            .evaluate(&context(&body))
            .expect("JSON evaluates");
        let Outcome::Transform { restore, .. } = verdict.outcome else {
            panic!("secrets must transform the body");
        };
        assert_eq!(restore.replacements.len(), 2);
        assert_eq!(verdict.findings.match_count, 3);
        assert_eq!(
            verdict.findings.metadata["detectors"],
            json!({"aws_access_key_id": 1, "github_token": 2})
        );
    }

    #[test]
    fn a_pem_block_inside_a_json_string_is_masked_as_one_secret() {
        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\\nb3BlbnNzaC1rZXktdjEAAAAA\\n-----END OPENSSH PRIVATE KEY-----";
        let body = Bytes::from(format!(
            r#"{{"messages":[{{"role":"user","content":"{pem}"}}]}}"#
        ));
        let verdict = policy(GuardrailsMode::Mask)
            .evaluate(&context(&body))
            .expect("JSON evaluates");
        let Outcome::Transform { body, restore } = verdict.outcome else {
            panic!("a private key must transform the body");
        };
        assert!(!std::str::from_utf8(&body).unwrap().contains("BEGIN"));
        assert!(restore.replacements[0].original.contains('\n'));
    }

    #[test]
    fn a_body_that_is_not_json_is_rejected_as_an_invalid_request() {
        let body = Bytes::from_static(b"\x00 definitely not json");
        let error = policy(GuardrailsMode::Mask)
            .evaluate(&context(&body))
            .err()
            .expect("garbage must not be forwarded silently");
        assert!(matches!(error, PolicyError::InvalidRequest(_)));
        assert!(error.to_string().contains("not JSON"));

        let array = Bytes::from_static(b"[1,2,3]");
        let error = policy(GuardrailsMode::Mask)
            .evaluate(&context(&array))
            .err()
            .expect("a non-object body cannot be scanned");
        assert!(matches!(error, PolicyError::InvalidRequest(_)));
    }

    #[test]
    fn declared_encodings_are_decoded_before_scanning() {
        use crate::compression::tests::{brotli, gzip};

        let plain = body_with(GITHUB_TOKEN);
        for (encoded, encoding) in [
            (brotli(&plain), "br"),
            (gzip(&plain), "gzip"),
            (plain.to_vec(), "identity"),
        ] {
            let verdict = policy(GuardrailsMode::Mask)
                .evaluate(&encoded_context(&encoded, encoding))
                .unwrap_or_else(|error| panic!("{encoding}: {error}"));
            let Outcome::Transform { body, .. } = verdict.outcome else {
                panic!("{encoding}: the secret must still be found");
            };
            let masked = std::str::from_utf8(&body).unwrap();
            assert!(
                masked.contains(&format!(
                    "export TOKEN={}",
                    policy(GuardrailsMode::Mask).placeholder(GITHUB_TOKEN)
                )),
                "{encoding}"
            );
        }
    }

    #[test]
    fn an_encoding_that_cannot_be_decoded_is_rejected_as_an_invalid_request() {
        for (body, encoding) in [
            (b"not gzip at all".as_slice(), "gzip"),
            (b"{}".as_slice(), "deflate"),
        ] {
            let error = policy(GuardrailsMode::Mask)
                .evaluate(&encoded_context(body, encoding))
                .err()
                .unwrap_or_else(|| panic!("{encoding} must fail closed"));
            assert!(
                matches!(error, PolicyError::InvalidRequest(_)),
                "{encoding}"
            );
        }
    }

    #[test]
    fn tool_credentials_and_attachment_data_are_left_alone() {
        let body = Bytes::from(format!(
            r#"{{"tools":[{{"type":"mcp","server_url":"https://x","authorization_token":"{GITHUB_TOKEN}","headers":{{"Authorization":"Bearer {GITHUB_TOKEN}"}},"description":"{AWS_KEY}"}}],"messages":[{{"role":"user","content":[{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{GITHUB_TOKEN}"}}}}]}}]}}"#
        ));
        let verdict = policy(GuardrailsMode::Mask)
            .evaluate(&context(&body))
            .expect("JSON evaluates");
        let Outcome::Transform { body, .. } = verdict.outcome else {
            panic!("the description secret must transform the body");
        };
        let masked = std::str::from_utf8(&body).unwrap();
        assert_eq!(masked.matches(GITHUB_TOKEN).count(), 3);
        assert_eq!(masked.matches(AWS_KEY).count(), 0);
        assert_eq!(verdict.findings.match_count, 1);
    }

    #[test]
    fn an_empty_body_is_allowed() {
        let body = Bytes::new();
        let verdict = policy(GuardrailsMode::Mask)
            .evaluate(&context(&body))
            .expect("empty bodies evaluate");
        assert!(matches!(verdict.outcome, Outcome::Allow));
    }
}
