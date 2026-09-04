use crate::providers::Provider;
use axum::{body::Bytes, http::StatusCode};
use std::{sync::Arc, time::Instant};

/// Evaluation metadata describes what a policy saw in a payload, so reading it
/// takes the same permission as reading the payload itself.
pub const EVALUATION_METADATA_SCOPE: &str = "payloads:read";

pub struct RequestContext<'a> {
    pub provider: Provider,
    pub model: Option<&'a str>,
    pub user_id: &'a str,
    pub key_id: &'a str,
    pub body: &'a Bytes,
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
    Block {
        status: StatusCode,
        code: &'static str,
    },
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
    fn evaluate(&self, context: &RequestContext<'_>) -> anyhow::Result<Verdict>;
}

pub struct NoopPolicy;

impl RequestPolicy for NoopPolicy {
    fn name(&self) -> &'static str {
        "noop"
    }

    fn version(&self) -> u32 {
        1
    }

    fn evaluate(&self, _context: &RequestContext<'_>) -> anyhow::Result<Verdict> {
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
    pub blocked: Option<Blocked>,
    pub restore: RestorationState,
    pub evaluations: Vec<Evaluation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub status: StatusCode,
    pub code: &'static str,
    pub policy: &'static str,
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
    pub error: anyhow::Error,
}

impl std::fmt::Display for PolicyFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "policy {} failed: {:#}", self.policy, self.error)
    }
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

    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    pub fn evaluate(&self, context: RequestContext<'_>) -> Result<Decision, PolicyFailure> {
        let mut body = context.body.clone();
        let mut restore = RestorationState::default();
        let mut evaluations = Vec::with_capacity(self.policies.len());
        for policy in self.policies.iter() {
            let started = Instant::now();
            let context = RequestContext {
                body: &body,
                ..context
            };
            let verdict = policy.evaluate(&context).map_err(|error| PolicyFailure {
                policy: policy.name(),
                error,
            })?;
            let duration_micros = started.elapsed().as_micros() as i64;
            let outcome_name = match &verdict.outcome {
                Outcome::Allow => "allow",
                Outcome::Block { .. } => "block",
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
                Outcome::Block { status, code } => {
                    return Ok(Decision {
                        body,
                        blocked: Some(Blocked {
                            status,
                            code,
                            policy: policy.name(),
                        }),
                        restore,
                        evaluations,
                    });
                }
                Outcome::Transform {
                    body: transformed,
                    restore: state,
                } => {
                    body = transformed;
                    restore.replacements.extend(state.replacements);
                }
            }
        }
        Ok(Decision {
            body,
            blocked: None,
            restore,
            evaluations,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(body: &Bytes) -> RequestContext<'_> {
        RequestContext {
            provider: Provider::Anthropic,
            model: Some("claude-test"),
            user_id: "user",
            key_id: "key",
            body,
        }
    }

    struct Blocker;

    impl RequestPolicy for Blocker {
        fn name(&self) -> &'static str {
            "blocker"
        }

        fn version(&self) -> u32 {
            3
        }

        fn evaluate(&self, _context: &RequestContext<'_>) -> anyhow::Result<Verdict> {
            Ok(Verdict {
                outcome: Outcome::Block {
                    status: StatusCode::FORBIDDEN,
                    code: "blocked_for_test",
                },
                findings: Findings {
                    severity: Some("high"),
                    match_count: 1,
                    metadata: serde_json::json!({"reason": "test"}),
                },
            })
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

        fn evaluate(&self, _context: &RequestContext<'_>) -> anyhow::Result<Verdict> {
            anyhow::bail!("detector exploded")
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

        assert!(decision.blocked.is_none());
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
    fn a_block_stops_the_pipeline_and_names_the_policy() {
        let body = Bytes::from_static(b"{}");
        let pipeline = Pipeline::new(vec![
            Arc::new(NoopPolicy),
            Arc::new(Blocker),
            Arc::new(NoopPolicy),
        ]);

        let decision = pipeline
            .evaluate(context(&body))
            .expect("block is not a failure");

        assert_eq!(
            decision.blocked,
            Some(Blocked {
                status: StatusCode::FORBIDDEN,
                code: "blocked_for_test",
                policy: "blocker",
            })
        );
        assert_eq!(decision.applied_policies(), "noop,blocker");
        assert_eq!(decision.evaluations[1].outcome, "block");
        assert_eq!(decision.evaluations[1].severity, Some("high"));
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
        assert!(failure.to_string().contains("detector exploded"));
    }
}
