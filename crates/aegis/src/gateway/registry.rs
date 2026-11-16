//! The gateway's in-memory view of stored providers and guardrails. It is
//! swapped whole on reload, so a request keeps the snapshot it started with.
use super::route::{Base, ProviderTarget, Upstream};
use crate::{
    config::{GuardrailsConfig, ProviderConfig, ProviderKind, default_vertex_base_url},
    policies::{Pipeline, pipeline},
    providers::Provider,
    settings::{SettingsStore, StoredProvider, effective_guardrails},
};
use anyhow::Context;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, RwLock},
};

#[derive(Default)]
pub(super) struct Snapshot {
    /// Keyed by owner (`None` for the instance) and provider ID.
    providers: HashMap<(Option<String>, String), ProviderTarget>,
    /// Each provider's type, keyed as `providers` is.
    types: HashMap<(Option<String>, String), &'static str>,
    /// The AWS region or Vertex location of each cloud provider whose
    /// endpoint names one.
    regions: HashMap<(Option<String>, String), String>,
    instance_guardrails: GuardrailsConfig,
    user_guardrails: HashMap<String, GuardrailsConfig>,
    /// Built on first use per user; compiling detectors is not free.
    pipelines: Mutex<HashMap<String, Pipeline>>,
}

impl Snapshot {
    pub(super) fn new(
        providers: Vec<StoredProvider>,
        instance_guardrails: GuardrailsConfig,
        user_guardrails: HashMap<String, GuardrailsConfig>,
    ) -> anyhow::Result<Self> {
        let mut types = HashMap::new();
        let mut regions = HashMap::new();
        let providers = providers
            .into_iter()
            .map(|stored| {
                let type_name = stored.config.kind.type_name();
                let kind = &stored.config.kind;
                let region = kind
                    .bedrock_region()
                    .or_else(|| kind.vertex_location())
                    .map(str::to_owned);
                let (id, target) = target(stored.config)?;
                types.insert((stored.owner.clone(), id.clone()), type_name);
                if let Some(region) = region {
                    regions.insert((stored.owner.clone(), id.clone()), region);
                }
                Ok(((stored.owner, id), target))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            providers,
            types,
            regions,
            instance_guardrails,
            user_guardrails,
            pipelines: Mutex::default(),
        })
    }

    pub(super) async fn load(store: &SettingsStore) -> anyhow::Result<Self> {
        Self::new(
            store.all_providers().await?,
            store.instance_guardrails().await?,
            store.all_user_guardrails().await?,
        )
    }

    /// Whether any layer of any user has this ID, so an unknown ID can be
    /// answered before authenticating.
    pub(super) fn exists(&self, id: &str) -> bool {
        self.providers.keys().any(|(_, known)| known == id)
    }

    pub(super) fn resolve(&self, user: &str, id: &str) -> Option<ProviderTarget> {
        self.providers
            .get(&(Some(user.to_owned()), id.to_owned()))
            .or_else(|| self.providers.get(&(None, id.to_owned())))
            .cloned()
    }

    /// The type of the provider `resolve` would pick.
    pub(super) fn type_name(&self, user: &str, id: &str) -> Option<&'static str> {
        self.types
            .get(&(Some(user.to_owned()), id.to_owned()))
            .or_else(|| self.types.get(&(None, id.to_owned())))
            .copied()
    }

    /// The AWS region or Vertex location of the provider `resolve` would
    /// pick, if its endpoint names one.
    pub(super) fn region(&self, user: &str, id: &str) -> Option<&str> {
        let own = (Some(user.to_owned()), id.to_owned());
        let key = if self.types.contains_key(&own) {
            own
        } else {
            (None, id.to_owned())
        };
        self.regions.get(&key).map(String::as_str)
    }

    /// The user's own providers and the instance's, sorted by ID.
    pub(super) fn visible(&self, user: &str) -> Vec<(String, &ProviderTarget)> {
        let mut visible: Vec<_> = self
            .providers
            .iter()
            .filter(|((owner, _), _)| owner.as_deref().is_none_or(|owner| owner == user))
            .map(|((_, id), target)| (id.clone(), target))
            .collect();
        visible.sort_by(|a, b| a.0.cmp(&b.0));
        visible
    }

    pub(super) fn pipeline(&self, user: &str, key: [u8; 32]) -> Pipeline {
        let mut pipelines = self.pipelines.lock().expect("pipeline cache poisoned");
        pipelines
            .entry(user.to_owned())
            .or_insert_with(|| {
                pipeline(
                    &effective_guardrails(
                        &self.instance_guardrails,
                        self.user_guardrails.get(user),
                    ),
                    key,
                )
            })
            .clone()
    }
}

pub(super) type Registry = Arc<RwLock<Arc<Snapshot>>>;

fn target(config: ProviderConfig) -> anyhow::Result<(String, ProviderTarget)> {
    let invalid = || format!("provider {:?} has an invalid base_url", config.id);
    let region = config.kind.bedrock_region();
    let fixed = |protocol, base_url: &str| -> anyhow::Result<Upstream> {
        Ok(Upstream::Fixed {
            protocol,
            base: Base::parse(base_url).with_context(invalid)?,
        })
    };
    let upstream = match &config.kind {
        ProviderKind::ClaudeSubscription { base_url } => fixed(Provider::Anthropic, base_url)?,
        ProviderKind::CodexSubscription { base_url } => fixed(Provider::Codex, base_url)?,
        ProviderKind::TypeSafe { base_url } => fixed(Provider::TypeSafe, base_url)?,
        ProviderKind::Bedrock { base_url } => {
            let runtime = Base::parse(base_url).with_context(invalid)?;
            let openai = match &region {
                Some(region) => Base::parse(&format!("https://bedrock-mantle.{region}.api.aws"))
                    .with_context(invalid)?,
                None => runtime.clone(),
            };
            Upstream::Bedrock { runtime, openai }
        }
        ProviderKind::Vertex { base_url } => Upstream::Vertex {
            custom: (*base_url != default_vertex_base_url())
                .then(|| Base::parse(base_url))
                .transpose()
                .with_context(invalid)?,
        },
    };
    let price_scope = match (&config.kind, region) {
        (ProviderKind::ClaudeSubscription { .. }, _) => "anthropic".to_owned(),
        (ProviderKind::CodexSubscription { .. }, _) => "openai".to_owned(),
        (ProviderKind::TypeSafe { .. }, _) => "typesafe".to_owned(),
        (ProviderKind::Bedrock { .. }, Some(region)) => format!("bedrock/{region}"),
        (ProviderKind::Bedrock { .. }, None) => "bedrock".to_owned(),
        (ProviderKind::Vertex { .. }, _) => "vertex_ai".to_owned(),
    };
    Ok((
        config.id,
        ProviderTarget {
            upstream,
            price_scope: Arc::from(price_scope),
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProviderKind;

    fn stored(owner: Option<&str>, id: &str, kind: ProviderKind) -> StoredProvider {
        StoredProvider {
            owner: owner.map(str::to_owned),
            config: ProviderConfig {
                id: id.into(),
                kind,
            },
        }
    }

    #[test]
    fn a_cloud_providers_region_is_its_endpoints_and_a_users_own_comes_first() {
        let bedrock = |url: &str| ProviderKind::Bedrock {
            base_url: url.into(),
        };
        let snapshot = Snapshot::new(
            vec![
                stored(
                    None,
                    "aws",
                    bedrock("https://bedrock-runtime.eu-west-1.amazonaws.com"),
                ),
                stored(Some("me"), "aws", bedrock("https://vpce.example.com")),
                stored(
                    None,
                    "us",
                    bedrock("https://bedrock-runtime.us-east-1.amazonaws.com"),
                ),
                stored(
                    None,
                    "gcp",
                    ProviderKind::Vertex {
                        base_url: "https://us-east5-aiplatform.googleapis.com".into(),
                    },
                ),
                stored(
                    None,
                    "global",
                    ProviderKind::Vertex {
                        base_url: crate::config::default_vertex_base_url(),
                    },
                ),
            ],
            GuardrailsConfig::default(),
            HashMap::new(),
        )
        .unwrap();
        assert_eq!(snapshot.region("you", "aws"), Some("eu-west-1"));
        assert_eq!(snapshot.region("me", "aws"), None);
        assert_eq!(snapshot.region("me", "us"), Some("us-east-1"));
        assert_eq!(snapshot.region("me", "missing"), None);
        assert_eq!(snapshot.region("me", "gcp"), Some("us-east5"));
        assert_eq!(snapshot.region("me", "global"), None);
    }
}
