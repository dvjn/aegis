use axum::body::Bytes;
use serde_json::{Map, Value};
use std::{sync::Arc, time::Instant};

pub mod detect;
pub mod mask;
pub mod restore;
pub mod secrets;
pub mod sse;

/// Evaluation metadata describes what a policy saw in a payload, so reading it
/// takes the same permission as reading the payload itself.
pub const EVALUATION_METADATA_SCOPE: &str = "payloads:read";

/// Items the provider seals or covers with a signature: `encrypted_content` on
/// an OpenAI reasoning item, `signature` on an Anthropic thinking block.
/// Rewriting any byte inside one makes the client's next replay of it fail
/// verification, so masking and restoration both leave the whole item alone.
const PROVIDER_SIGNED_ITEM_TYPES: [&str; 3] = ["reasoning", "thinking", "redacted_thinking"];

fn is_provider_signed_item(fields: &Map<String, Value>) -> bool {
    fields
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| PROVIDER_SIGNED_ITEM_TYPES.contains(&kind))
}

#[derive(Clone)]
pub struct RequestContext {
    pub body: Bytes,
    pub content_encoding: Option<String>,
}

#[derive(Debug)]
pub enum PolicyError {
    InvalidRequest(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for PolicyError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl From<serde_json::Error> for PolicyError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(error.into())
    }
}

impl std::fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(formatter, "invalid request: {message}"),
            Self::Internal(error) => write!(formatter, "{error:#}"),
        }
    }
}

#[derive(Debug, Default)]
pub struct RestorationState {
    pub replacements: Vec<Replacement>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Replacement {
    pub placeholder: String,
    pub original: String,
}

pub enum Outcome {
    Allow,
    Transform {
        body: Bytes,
        restore: RestorationState,
    },
}

#[derive(Debug, Default)]
pub struct Findings {
    pub severity: Option<&'static str>,
    pub match_count: i64,
    pub metadata: serde_json::Value,
}

pub struct Verdict {
    pub outcome: Outcome,
    pub findings: Findings,
}

impl Verdict {
    pub fn allow() -> Self {
        Self {
            outcome: Outcome::Allow,
            findings: Findings::default(),
        }
    }
}

pub trait RequestPolicy: Send + Sync {
    fn name(&self) -> &'static str;
    fn version(&self) -> u32;
    fn evaluate(&self, context: &RequestContext) -> Result<Verdict, PolicyError>;
}

#[cfg(test)]
pub struct NoopPolicy;

#[cfg(test)]
impl RequestPolicy for NoopPolicy {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn version(&self) -> u32 {
        1
    }

    fn evaluate(&self, _context: &RequestContext) -> Result<Verdict, PolicyError> {
        Ok(Verdict::allow())
    }
}

#[derive(Debug, Clone)]
pub struct Evaluation {
    pub policy: &'static str,
    pub policy_version: u32,
    pub outcome: &'static str,
    pub severity: Option<&'static str>,
    pub match_count: i64,
    pub duration_micros: i64,
    pub metadata: serde_json::Value,
}

pub struct Decision {
    pub body: Bytes,
    pub restore: RestorationState,
    pub evaluations: Vec<Evaluation>,
}

impl Decision {}

#[derive(Debug)]
pub struct PolicyFailure {
    pub policy: &'static str,
    pub error: PolicyError,
}

impl std::fmt::Display for PolicyFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "policy {} failed: {}", self.policy, self.error)
    }
}

pub fn pipeline(guardrails: &crate::config::GuardrailsConfig, key: [u8; 32]) -> Pipeline {
    if !guardrails.enabled {
        return Pipeline::default();
    }
    let mode = guardrails.mode;
    let built: [(bool, mask::MaskingPolicy); 2] = [
        (
            guardrails.secrets.enabled,
            mask::MaskingPolicy::secrets(&guardrails.secrets, key, mode),
        ),
        (
            guardrails.regex.enabled,
            mask::MaskingPolicy::regex(&guardrails.regex, key, mode),
        ),
    ];
    Pipeline::new(
        built
            .into_iter()
            .filter(|(enabled, policy)| *enabled && !policy.has_no_detectors())
            .map(|(_, policy)| Arc::new(policy) as Arc<dyn RequestPolicy>)
            .collect(),
    )
}

#[derive(Clone, Default)]
pub struct Pipeline {
    policies: Arc<[Arc<dyn RequestPolicy>]>,
}

impl Pipeline {
    pub fn new(policies: Vec<Arc<dyn RequestPolicy>>) -> Self {
        Self {
            policies: policies.into(),
        }
    }

    pub fn evaluate(&self, mut context: RequestContext) -> Result<Decision, PolicyFailure> {
        let mut restore = RestorationState::default();
        let mut evaluations = Vec::with_capacity(self.policies.len());
        for policy in self.policies.iter() {
            let started = Instant::now();
            let verdict = policy.evaluate(&context).map_err(|error| PolicyFailure {
                policy: policy.name(),
                error,
            })?;
            let duration_micros = started.elapsed().as_micros() as i64;
            let outcome_name = match &verdict.outcome {
                Outcome::Allow => "allow",
                Outcome::Transform { .. } => "transform",
            };
            evaluations.push(Evaluation {
                policy: policy.name(),
                policy_version: policy.version(),
                outcome: outcome_name,
                severity: verdict.findings.severity,
                match_count: verdict.findings.match_count,
                duration_micros,
                metadata: verdict.findings.metadata,
            });
            match verdict.outcome {
                Outcome::Allow => {}
                Outcome::Transform {
                    body: transformed,
                    restore: state,
                } => {
                    context.body = transformed;
                    context.content_encoding = None;
                    restore.replacements.extend(state.replacements);
                }
            }
        }
        Ok(Decision {
            body: context.body,
            restore,
            evaluations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(body: &Bytes) -> RequestContext {
        RequestContext {
            body: body.clone(),
            content_encoding: None,
        }
    }

    struct Failing;

    impl RequestPolicy for Failing {
        fn name(&self) -> &'static str {
            "failing"
        }

        fn version(&self) -> u32 {
            1
        }

        fn evaluate(&self, _context: &RequestContext) -> Result<Verdict, PolicyError> {
            Err(anyhow::anyhow!("detector exploded").into())
        }
    }

    #[test]
    fn the_pipeline_holds_the_guardrails_the_configuration_turns_on() {
        use crate::config::{
            GuardrailConfig, GuardrailsConfig, GuardrailsMode, RegexGuardrailConfig,
        };
        use std::collections::BTreeMap;

        let built = |guardrails: GuardrailsConfig| {
            pipeline(&guardrails, [7; 32])
                .policies
                .iter()
                .map(|policy| policy.name())
                .collect::<Vec<_>>()
        };
        let on = |detectors: Option<Vec<String>>| GuardrailConfig {
            enabled: true,
            detectors,
        };
        let off = GuardrailConfig {
            enabled: false,
            detectors: None,
        };

        assert!(
            built(GuardrailsConfig::default()).is_empty(),
            "guardrails are off until they are turned on"
        );
        assert_eq!(
            built(GuardrailsConfig {
                enabled: true,
                mode: GuardrailsMode::Mask,
                secrets: on(None),
                regex: RegexGuardrailConfig::default(),
            }),
            ["secrets"]
        );
        assert_eq!(
            built(GuardrailsConfig {
                enabled: true,
                mode: GuardrailsMode::Mask,
                secrets: on(None),
                regex: RegexGuardrailConfig {
                    enabled: true,
                    detectors: BTreeMap::from([(
                        "internal_token".to_owned(),
                        r"\bint_[a-z0-9]{32}\b".to_owned(),
                    )]),
                },
            }),
            ["secrets", "regex"]
        );
        assert_eq!(
            built(GuardrailsConfig {
                enabled: true,
                mode: GuardrailsMode::Mask,
                secrets: off.clone(),
                regex: RegexGuardrailConfig {
                    enabled: true,
                    detectors: BTreeMap::from([(
                        "internal_token".to_owned(),
                        r"\bint_[a-z0-9]{32}\b".to_owned(),
                    )]),
                },
            }),
            ["regex"],
            "each guardrail is turned on by itself"
        );
        assert!(
            built(GuardrailsConfig {
                enabled: true,
                mode: GuardrailsMode::Mask,
                secrets: on(Some(Vec::new())),
                regex: RegexGuardrailConfig {
                    enabled: true,
                    detectors: BTreeMap::new(),
                },
            })
            .is_empty(),
            "a guardrail with every detector disabled is not run at all"
        );
    }

    #[test]
    fn the_regex_guardrail_runs_after_the_built_in_ones() {
        use crate::config::{
            GuardrailConfig, GuardrailsConfig, GuardrailsMode, RegexGuardrailConfig,
        };
        use std::collections::BTreeMap;

        let guardrails = GuardrailsConfig {
            enabled: true,
            mode: GuardrailsMode::Mask,
            secrets: GuardrailConfig::on(),
            regex: RegexGuardrailConfig {
                enabled: true,
                detectors: BTreeMap::from([(
                    "internal_token".to_owned(),
                    r"\bint_[a-z0-9]{32}\b".to_owned(),
                )]),
            },
        };
        let body = Bytes::from_static(
            br#"{"messages":[{"role":"user","content":"int_0123456789abcdef0123456789abcdef"}]}"#,
        );
        let decision = pipeline(&guardrails, [7; 32])
            .evaluate(context(&body))
            .expect("JSON evaluates");
        assert_eq!(decision.evaluations.len(), 2);
        assert_eq!(decision.restore.replacements.len(), 1);
        assert!(
            std::str::from_utf8(&decision.body)
                .unwrap()
                .contains("AEGIS_MASKED_INTERNAL_TOKEN_")
        );
    }

    #[test]
    fn evaluation_metadata_is_read_under_the_superuser_only_payload_scope() {
        use crate::domain::{SCOPES, role_allows_scope};

        assert!(SCOPES.contains(&EVALUATION_METADATA_SCOPE));
        assert!(role_allows_scope(EVALUATION_METADATA_SCOPE, "superuser"));
        assert!(!role_allows_scope(EVALUATION_METADATA_SCOPE, "user"));
    }

    #[test]
    fn the_noop_policy_forwards_the_original_bytes_unchanged() {
        let body = Bytes::from_static(br#"{"model":"claude-test","messages":[]}"#);
        let pipeline = Pipeline::new(vec![Arc::new(NoopPolicy)]);

        let decision = pipeline.evaluate(context(&body)).expect("noop never fails");

        assert_eq!(decision.body, body);
        assert_eq!(decision.body.as_ptr(), body.as_ptr());
        assert!(decision.restore.replacements.is_empty());
        let [evaluation] = decision.evaluations.as_slice() else {
            panic!("one evaluation should be recorded");
        };
        assert_eq!(evaluation.policy, "noop");
        assert_eq!(evaluation.policy_version, 1);
        assert_eq!(evaluation.outcome, "allow");
        assert_eq!(evaluation.match_count, 0);
    }

    #[test]
    fn an_empty_pipeline_allows_and_records_nothing() {
        let body = Bytes::from_static(b"{}");
        let decision = Pipeline::default()
            .evaluate(context(&body))
            .expect("empty pipeline never fails");

        assert!(decision.evaluations.is_empty());
        assert_eq!(decision.body.as_ptr(), body.as_ptr());
    }

    #[test]
    fn a_policy_error_fails_closed_with_the_policy_name() {
        let body = Bytes::from_static(b"{}");
        let pipeline = Pipeline::new(vec![Arc::new(Failing)]);

        let failure = pipeline
            .evaluate(context(&body))
            .err()
            .expect("the failing policy should surface its error");

        assert_eq!(failure.policy, "failing");
        assert!(matches!(failure.error, PolicyError::Internal(_)));
        assert!(failure.to_string().contains("detector exploded"));
    }
}
