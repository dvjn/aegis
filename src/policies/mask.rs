use super::{
    Findings, Outcome, PolicyError, Replacement, RequestContext, RequestPolicy, RestorationState,
    Verdict,
    detect::{DetectorSet, select},
    is_provider_signed_item,
};
use crate::{
    compression::decode_declared,
    config::{GuardrailConfig, GuardrailsMode, RegexGuardrailConfig},
};
use axum::body::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use std::collections::BTreeMap;

/// A placeholder reads `AEGIS_MASKED_<DETECTOR>_<digest>_END`, so the provider
/// can tell that it sees a masked value and what kind it stands in for.
pub const PLACEHOLDER_PREFIX: &str = "AEGIS_MASKED_";
pub const PLACEHOLDER_SUFFIX: &str = "_END";
const PLACEHOLDER_DIGEST_BYTES: usize = 11;

const SCANNED_FIELDS: [&str; 5] = ["system", "messages", "tools", "instructions", "input"];
const TOOL_CREDENTIAL_FIELDS: [&str; 2] = ["headers", "authorization_token"];
const SIGNED_ITEM_CONTAINERS: [&str; 2] = ["messages", "input"];
const ATTACHMENT_DATA_FIELD: &str = "data";

const SEVERITY: &str = "high";

/// Masks whatever its detectors find. One instance per guardrail: the name it
/// reports and the patterns it carries are all that separate one from another.
pub struct MaskingPolicy {
    name: &'static str,
    detectors: DetectorSet,
    placeholder_key: [u8; 32],
    mode: GuardrailsMode,
}

impl MaskingPolicy {
    pub fn secrets(
        config: &GuardrailConfig,
        placeholder_key: [u8; 32],
        mode: GuardrailsMode,
    ) -> Self {
        Self {
            name: "secrets",
            detectors: DetectorSet::new(select(
                &super::secrets::DETECTORS,
                config.detectors.as_deref(),
            )),
            placeholder_key,
            mode,
        }
    }

    /// User-defined patterns were compiled once already when the configuration
    /// was validated, so a failure here is a programming error rather than bad input.
    pub fn regex(
        config: &RegexGuardrailConfig,
        placeholder_key: [u8; 32],
        mode: GuardrailsMode,
    ) -> Self {
        Self {
            name: "regex",
            detectors: DetectorSet::regex(&config.detectors)
                .expect("regex detectors were validated when the configuration loaded"),
            placeholder_key,
            mode,
        }
    }

    pub fn has_no_detectors(&self) -> bool {
        self.detectors.is_empty()
    }

    pub fn placeholder(&self, detector: &str, secret: &str) -> String {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.placeholder_key)
            .expect("HMAC accepts any key");
        mac.update(secret.as_bytes());
        let digest = mac.finalize().into_bytes();
        let mut placeholder = String::with_capacity(
            PLACEHOLDER_PREFIX.len()
                + detector.len()
                + 1
                + 2 * PLACEHOLDER_DIGEST_BYTES
                + PLACEHOLDER_SUFFIX.len(),
        );
        placeholder.push_str(PLACEHOLDER_PREFIX);
        placeholder.push_str(&detector.to_ascii_uppercase());
        placeholder.push('_');
        for byte in &digest[..PLACEHOLDER_DIGEST_BYTES] {
            placeholder.push_str(&format!("{byte:02x}"));
        }
        placeholder.push_str(PLACEHOLDER_SUFFIX);
        placeholder
    }
}

#[derive(Default)]
struct Scan {
    detector_counts: BTreeMap<String, i64>,
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

impl MaskingPolicy {
    fn mask_string(&self, text: &mut String, scan: &mut Scan) {
        let findings = self.detectors.find(text);
        if findings.is_empty() {
            return;
        }
        let mut masked = String::with_capacity(text.len());
        let mut cursor = 0;
        for finding in &findings {
            *scan
                .detector_counts
                .entry(finding.detector.to_owned())
                .or_insert(0) += 1;
            let placeholder = self.placeholder(finding.detector, finding.secret);
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

    fn mask_value(
        &self,
        value: &mut Value,
        scan: &mut Scan,
        skip: &[&str],
        skip_signed_items: bool,
    ) {
        match value {
            Value::String(text) => self.mask_string(text, scan),
            Value::Array(items) => {
                for item in items {
                    self.mask_value(item, scan, skip, skip_signed_items);
                }
            }
            Value::Object(fields) => {
                if skip_signed_items && is_provider_signed_item(fields) {
                    return;
                }
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
                    self.mask_value(field, scan, skip, skip_signed_items);
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
            let skip_signed_items = SIGNED_ITEM_CONTAINERS.contains(&name);
            if let Some(value) = fields.get_mut(name) {
                self.mask_value(value, &mut scan, skip, skip_signed_items);
            }
        }
        scan
    }
}

impl RequestPolicy for MaskingPolicy {
    fn name(&self) -> &'static str {
        self.name
    }

    fn version(&self) -> u32 {
        1
    }

    fn evaluate(&self, context: &RequestContext) -> Result<Verdict, PolicyError> {
        if context.body.is_empty() || self.detectors.is_empty() {
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
            severity: Some(SEVERITY),
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

    fn every_detector() -> GuardrailConfig {
        GuardrailConfig {
            enabled: true,
            detectors: None,
        }
    }

    fn policy(mode: GuardrailsMode) -> MaskingPolicy {
        MaskingPolicy::secrets(&every_detector(), [7; 32], mode)
    }

    const INTERNAL_TOKEN: &str = "int_0123456789abcdef0123456789abcdef";

    fn regex_policy(mode: GuardrailsMode) -> MaskingPolicy {
        MaskingPolicy::regex(
            &RegexGuardrailConfig {
                enabled: true,
                detectors: BTreeMap::from([
                    (
                        "internal_token".to_owned(),
                        r"\bint_[a-z0-9]{32}\b".to_owned(),
                    ),
                    ("employee_id".to_owned(), r"\bEMP-\d{6}\b".to_owned()),
                ]),
            },
            [7; 32],
            mode,
        )
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
    fn placeholders_name_their_detector_and_are_stable_per_secret() {
        let policy = policy(GuardrailsMode::Mask);
        let first = policy.placeholder("github_token", GITHUB_TOKEN);
        assert_eq!(
            first.len(),
            "AEGIS_MASKED_GITHUB_TOKEN_".len() + 22 + "_END".len()
        );
        assert!(first.starts_with("AEGIS_MASKED_GITHUB_TOKEN_"));
        assert!(first.ends_with(PLACEHOLDER_SUFFIX));
        assert_eq!(first, policy.placeholder("github_token", GITHUB_TOKEN));
        assert_ne!(first, policy.placeholder("aws_access_key_id", AWS_KEY));
        assert_ne!(
            first,
            MaskingPolicy::secrets(&every_detector(), [8; 32], GuardrailsMode::Mask)
                .placeholder("github_token", GITHUB_TOKEN)
        );
        let same_secret_other_name = policy.placeholder("npm_token", GITHUB_TOKEN);
        assert_ne!(first, same_secret_other_name);
        assert_eq!(
            &first[first.len() - 26..],
            &same_secret_other_name[same_secret_other_name.len() - 26..],
            "the digest depends on the secret alone"
        );
    }

    #[test]
    fn no_detector_matches_a_placeholder_of_any_kind() {
        let secrets = policy(GuardrailsMode::Mask);
        let regex = regex_policy(GuardrailsMode::Mask);
        let names = super::super::secrets::DETECTORS
            .iter()
            .map(|detector| detector.name)
            .chain(["internal_token", "employee_id"]);
        for name in names {
            let placeholder = secrets.placeholder(name, GITHUB_TOKEN);
            let text = format!("run export X={placeholder} now");
            assert!(secrets.detectors.find(&text).is_empty(), "{placeholder}");
            assert!(regex.detectors.find(&text).is_empty(), "{placeholder}");
        }
    }

    #[test]
    fn a_regex_detector_masks_its_match_under_its_own_name() {
        let body = body_with(INTERNAL_TOKEN);
        let policy = regex_policy(GuardrailsMode::Mask);
        let verdict = policy.evaluate(&context(&body)).expect("JSON evaluates");
        let Outcome::Transform { body, restore } = verdict.outcome else {
            panic!("a regex match must transform the body");
        };
        let placeholder = policy.placeholder("internal_token", INTERNAL_TOKEN);
        assert!(placeholder.starts_with("AEGIS_MASKED_INTERNAL_TOKEN_"));
        assert_eq!(
            placeholder.len(),
            "AEGIS_MASKED_INTERNAL_TOKEN_".len() + 22 + "_END".len()
        );
        let masked = std::str::from_utf8(&body).unwrap();
        assert!(masked.contains(&format!("export TOKEN={placeholder} please")));
        assert_eq!(
            restore.replacements,
            [Replacement {
                placeholder,
                original: INTERNAL_TOKEN.to_owned(),
            }]
        );
        assert_eq!(policy.name(), "regex");
        assert_eq!(verdict.findings.severity, Some("high"));
        assert_eq!(
            verdict.findings.metadata["detectors"],
            json!({"internal_token": 1})
        );
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
        let placeholder = policy(GuardrailsMode::Mask).placeholder("github_token", GITHUB_TOKEN);
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
        let placeholders: Vec<&str> = restore
            .replacements
            .iter()
            .map(|replacement| replacement.placeholder.as_str())
            .collect();
        assert!(
            placeholders
                .iter()
                .any(|placeholder| placeholder.starts_with("AEGIS_MASKED_GITHUB_TOKEN_"))
        );
        assert!(
            placeholders
                .iter()
                .any(|placeholder| placeholder.starts_with("AEGIS_MASKED_AWS_ACCESS_KEY_ID_"))
        );
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
                    policy(GuardrailsMode::Mask).placeholder("github_token", GITHUB_TOKEN)
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

    const REASONING_ID: &str = "rs_0ec282262dc0e3bb016aa1809b0f0087d09c419d9e786d1541";
    const BASE64_ALPHABET: &str =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const BASE64URL_ALPHABET: &str =
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

    fn deterministic_blob(seed: u64, length: usize, alphabet: &str) -> String {
        let symbols: Vec<char> = alphabet.chars().collect();
        let mut state = seed;
        (0..length)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                symbols[(state >> 33) as usize % symbols.len()]
            })
            .collect()
    }

    // Assembled at runtime so the fixture is not itself flagged as a live key.
    fn aws_key_shaped() -> String {
        format!("AKIA{}", "ABCDEFGHIJKLMNOP")
    }

    fn responses_body(encrypted_content: &str) -> Bytes {
        Bytes::from(
            json!({
                "model": "gpt-5",
                "input": [
                    {
                        "type": "reasoning",
                        "id": REASONING_ID,
                        "encrypted_content": encrypted_content,
                        "summary": [],
                    },
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hello"}],
                    },
                ],
            })
            .to_string(),
        )
    }

    fn reasoning_item(body: &Bytes) -> Value {
        serde_json::from_slice::<Value>(body).expect("the masked body is JSON")["input"][0].clone()
    }

    fn reasoning_blobs() -> [(&'static str, String); 4] {
        [
            (
                "standard base64, trailing padding",
                format!("{}==", deterministic_blob(1, 510, BASE64_ALPHABET)),
            ),
            (
                "standard base64, no padding",
                deterministic_blob(2, 684, BASE64_ALPHABET),
            ),
            (
                "base64url with - and _",
                deterministic_blob(3, 640, BASE64URL_ALPHABET),
            ),
            (
                "base64url, single = pad",
                format!("{}=", deterministic_blob(4, 767, BASE64URL_ALPHABET)),
            ),
        ]
    }

    #[test]
    fn responses_reasoning_items_pass_through_untouched_unless_a_detector_matches() {
        for (shape, blob) in reasoning_blobs() {
            let body = responses_body(&blob);
            let verdict = policy(GuardrailsMode::Mask)
                .evaluate(&context(&body))
                .unwrap_or_else(|error| panic!("{shape}: {error}"));
            assert!(
                matches!(verdict.outcome, Outcome::Allow),
                "{shape}: no secrets detector matches an opaque base64 blob"
            );
            assert_eq!(verdict.findings.match_count, 0, "{shape}");
        }
    }

    #[test]
    fn a_vendor_prefix_inside_encrypted_content_survives_masking() {
        let head = deterministic_blob(5, 300, BASE64URL_ALPHABET);
        let tail = deterministic_blob(6, 200, BASE64URL_ALPHABET);
        let body = responses_body(&format!("{head}-sk-{tail}"));
        let verdict = policy(GuardrailsMode::Mask)
            .evaluate(&context(&body))
            .expect("JSON evaluates");
        assert!(
            matches!(verdict.outcome, Outcome::Allow),
            "a sealed reasoning item is never rewritten"
        );
        assert_eq!(verdict.findings.match_count, 0);
    }

    #[test]
    fn a_sealed_reasoning_item_survives_while_a_secret_beside_it_is_masked() {
        let head = deterministic_blob(7, 300, BASE64URL_ALPHABET);
        let tail = deterministic_blob(8, 200, BASE64URL_ALPHABET);
        let secret = aws_key_shaped();
        let reasoning = json!({
            "type": "reasoning",
            "id": REASONING_ID,
            "encrypted_content": format!("{head}-sk-{tail}"),
            "summary": [],
        });
        let body = Bytes::from(
            json!({
                "model": "gpt-5",
                "input": [
                    reasoning.clone(),
                    {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": format!("key {secret}")}],
                    },
                ],
            })
            .to_string(),
        );
        let policy = policy(GuardrailsMode::Mask);
        let verdict = policy.evaluate(&context(&body)).expect("JSON evaluates");
        let Outcome::Transform { body: masked, .. } = verdict.outcome else {
            panic!("the secret beside the reasoning item must transform the body");
        };
        assert_eq!(reasoning_item(&masked), reasoning);
        let masked_text = std::str::from_utf8(&masked).unwrap();
        assert!(!masked_text.contains(&secret));
        assert!(masked_text.contains(&policy.placeholder("aws_access_key_id", &secret)));
        assert_eq!(
            verdict.findings.metadata["detectors"],
            json!({"aws_access_key_id": 1})
        );
    }

    #[test]
    fn an_anthropic_thinking_block_and_its_signature_survive_masking() {
        let secret = aws_key_shaped();
        let thinking = json!({
            "type": "thinking",
            "thinking": format!("the operator pasted {secret}"),
            "signature": deterministic_blob(9, 256, BASE64_ALPHABET),
        });
        let body = Bytes::from(
            json!({
                "model": "claude-test",
                "messages": [
                    {"role": "assistant", "content": [thinking.clone()]},
                    {"role": "user", "content": [{"type": "text", "text": format!("use {secret}")}]},
                ],
            })
            .to_string(),
        );
        let policy = policy(GuardrailsMode::Mask);
        let verdict = policy.evaluate(&context(&body)).expect("JSON evaluates");
        let Outcome::Transform { body: masked, .. } = verdict.outcome else {
            panic!("the secret in the user message must transform the body");
        };
        let document: Value = serde_json::from_slice(&masked).expect("the masked body is JSON");
        assert_eq!(document["messages"][0]["content"][0], thinking);
        let masked_text = std::str::from_utf8(&masked).unwrap();
        assert_eq!(masked_text.matches(&secret).count(), 1);
        assert!(masked_text.contains(&format!(
            "use {}",
            policy.placeholder("aws_access_key_id", &secret)
        )));
        assert_eq!(verdict.findings.match_count, 1);
    }

    #[test]
    fn the_reasoning_item_id_matches_no_secrets_detector() {
        let text = format!("id {REASONING_ID} here");
        assert!(
            policy(GuardrailsMode::Mask)
                .detectors
                .find(&text)
                .is_empty(),
            "{REASONING_ID}"
        );
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
