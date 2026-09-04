use axum::body::Bytes;
use std::{sync::Arc, time::Instant};

pub mod mask;
pub mod secrets;

/// Evaluation metadata describes what a policy saw in a payload, so reading it
/// takes the same permission as reading the payload itself.
pub const EVALUATION_METADATA_SCOPE: &str = "payloads:read";

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

impl Decision {
    pub fn applied_policies(&self) -> String {
        self.evaluations
            .iter()
            .map(|evaluation| evaluation.policy)
            .collect::<Vec<_>>()
            .join(",")
    }
}

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

pub fn pipeline(config: &crate::config::Config) -> Pipeline {
    if !config.guardrails.enabled {
        return Pipeline::default();
    }
    Pipeline::new(vec![Arc::new(mask::SecretsPolicy::new(
        config.secret_placeholder_key,
        config.guardrails.mode,
    ))])
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
        assert_eq!(decision.applied_policies(), "noop");
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
        assert_eq!(decision.applied_policies(), "");
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
