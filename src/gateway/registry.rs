//! The gateway's in-memory view of stored providers and guardrails. It is
//! swapped whole on reload, so a request keeps the snapshot it started with.
use super::ProviderTarget;
use crate::{
    config::{GuardrailsConfig, ProviderConfig, ProviderKind},
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
        let providers = providers
            .into_iter()
            .map(|stored| {
                let (id, target) = target(stored.config)?;
                Ok(((stored.owner, id), target))
            })
            .collect::<anyhow::Result<_>>()?;
        Ok(Self {
            providers,
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

    /// The user's own providers and the instance's, sorted by ID.
    pub(super) fn visible(&self, user: &str) -> Vec<(String, Provider)> {
        let mut visible: Vec<_> = self
            .providers
            .iter()
            .filter(|((owner, _), _)| owner.as_deref().is_none_or(|owner| owner == user))
            .map(|((_, id), target)| (id.clone(), target.kind))
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
    let (kind, base_url) = match config.kind {
        ProviderKind::ClaudeSubscription { base_url } => (Provider::Anthropic, base_url),
        ProviderKind::CodexSubscription { base_url } => (Provider::Codex, base_url),
        ProviderKind::TypeSafe { base_url } => (Provider::TypeSafe, base_url),
    };
    let parsed = url::Url::parse(&base_url)
        .with_context(|| format!("provider {:?} has an invalid base_url", config.id))?;
    Ok((
        config.id,
        ProviderTarget {
            kind,
            origin: Arc::from(parsed.origin().ascii_serialization()),
            base_path: Arc::from(parsed.path().trim_end_matches('/')),
        },
    ))
}
