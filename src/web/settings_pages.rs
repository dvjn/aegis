//! Provider and guardrail settings pages. Every route exists twice: under
//! `/settings` for the signed-in user's own layer, and under `/admin` for the
//! instance layer, which only superusers reach.
use super::{
    AppState, authenticate, csrf_from_token,
    error::WebError,
    pages::{account_principal, is_superuser},
    views::{
        self,
        settings::{GuardrailsView, Layer, NewProvider, ProvidersView},
    },
};
use crate::{
    config::{GuardrailConfig, GuardrailsConfig, GuardrailsMode, ProviderConfig, ProviderKind},
    domain::Principal,
    policies::{detect, secrets},
    settings::{Scope, SettingsError, effective_guardrails},
};
use axum::{
    Form, Router,
    extract::{Path, Query, RawForm, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_extra::extract::cookie::CookieJar;
use serde::Deserialize;
use std::collections::BTreeMap;

pub(super) fn routes() -> Router<AppState> {
    let mut router = Router::new();
    for layer in [Layer::Mine, Layer::Instance] {
        let base = layer.base();
        router = router
            .route(
                &format!("{base}/providers"),
                get(move |state, jar, query| providers_page(layer, state, jar, query))
                    .post(move |state, jar, form| provider_create(layer, state, jar, form)),
            )
            .route(
                &format!("{base}/providers/{{id}}"),
                post(move |path, state, jar, form| provider_update(layer, path, state, jar, form)),
            )
            .route(
                &format!("{base}/providers/{{id}}/move"),
                post(move |path, state, jar, form| provider_move(layer, path, state, jar, form)),
            )
            .route(
                &format!("{base}/providers/{{id}}/delete"),
                post(move |path, state, jar, form| provider_delete(layer, path, state, jar, form)),
            )
            .route(
                &format!("{base}/guardrails"),
                get(move |state, jar| guardrails_page(layer, state, jar))
                    .post(move |state, jar, form| guardrails_save(layer, state, jar, form)),
            )
            .route(
                &format!("{base}/guardrails/test"),
                post(move |state, jar, form| guardrails_test(layer, state, jar, form)),
            );
    }
    router
}

fn scope(layer: Layer, principal: &Principal) -> Result<Scope, WebError> {
    match layer {
        Layer::Mine => Ok(Scope::User(principal.user_id())),
        Layer::Instance if is_superuser(principal) => Ok(Scope::Instance),
        Layer::Instance => Err(WebError::forbidden()),
    }
}

/// The principal of a form submission, after checking its CSRF token.
async fn submitter(
    state: &AppState,
    jar: &CookieJar,
    csrf: &str,
    layer: Layer,
) -> Result<Option<(Principal, Scope)>, WebError> {
    csrf_from_token(state, csrf, jar)?;
    let Ok(principal) = authenticate(state, jar, Some(csrf)).await else {
        return Ok(None);
    };
    let scope = scope(layer, &principal)?;
    Ok(Some((principal, scope)))
}

/// Applies a change to the running gateway now rather than at the next
/// periodic refresh.
async fn apply(state: &AppState) {
    if let Err(error) = state.gateway.reload(&state.settings).await {
        tracing::error!(%error, "failed to reload providers and guardrails");
    }
}

fn to_login() -> Response {
    Redirect::to("/login").into_response()
}

#[derive(Deserialize)]
struct ProvidersQuery {
    edit: Option<String>,
    delete: Option<String>,
}

#[derive(Default)]
struct ProvidersMessages<'a> {
    editing: Option<&'a str>,
    deleting: Option<&'a str>,
    new: Option<NewProvider>,
    error: Option<&'a str>,
    notice: Option<&'a str>,
}

async fn render_providers(
    state: &AppState,
    principal: &Principal,
    csrf: &str,
    layer: Layer,
    messages: ProvidersMessages<'_>,
) -> Result<maud::Markup, WebError> {
    let scope = scope(layer, principal)?;
    let settings = &state.settings;
    let providers = settings
        .providers(scope)
        .await
        .map_err(WebError::internal)?;
    let inherited = match layer {
        Layer::Mine => settings
            .providers(Scope::Instance)
            .await
            .map_err(WebError::internal)?,
        Layer::Instance => Vec::new(),
    };
    let grants = settings
        .grant_counts(scope)
        .await
        .map_err(WebError::internal)?;
    let new = messages.new.unwrap_or_default();
    // An instance provider's users can get a copy instead of losing it.
    let users_of_deleting: Vec<String> = match (layer, messages.deleting) {
        (Layer::Instance, Some(id)) if providers.iter().any(|p| p.id == id) => settings
            .instance_provider_users(id)
            .await
            .map_err(WebError::internal)?
            .into_iter()
            .map(|(_, email)| email)
            .collect(),
        _ => Vec::new(),
    };
    Ok(views::settings::providers_page(&ProvidersView {
        csrf,
        layer,
        superuser: is_superuser(principal),
        providers: &providers,
        inherited: &inherited,
        grants: &grants,
        editing: messages.editing,
        deleting: messages.deleting,
        users_of_deleting: &users_of_deleting,
        new: &new,
        error: messages.error,
        notice: messages.notice,
    }))
}

async fn providers_page(
    layer: Layer,
    State(state): State<AppState>,
    jar: CookieJar,
    Query(query): Query<ProvidersQuery>,
) -> Result<Response, WebError> {
    let Some((principal, csrf)) = account_principal(&state, &jar).await else {
        return Ok(to_login());
    };
    Ok(render_providers(
        &state,
        &principal,
        &csrf,
        layer,
        ProvidersMessages {
            editing: query.edit.as_deref(),
            deleting: query.delete.as_deref(),
            ..Default::default()
        },
    )
    .await?
    .into_response())
}

#[derive(Deserialize)]
struct CreateProviderForm {
    csrf: String,
    id: String,
    kind: String,
    #[serde(default)]
    base_url: String,
}

fn optional(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

async fn provider_create(
    layer: Layer,
    State(state): State<AppState>,
    jar: CookieJar,
    Form(input): Form<CreateProviderForm>,
) -> Result<Response, WebError> {
    let Some((principal, scope)) = submitter(&state, &jar, &input.csrf, layer).await? else {
        return Ok(to_login());
    };
    let id = input.id.trim().to_owned();
    let result = match ProviderKind::from_parts(&input.kind, optional(&input.base_url)) {
        Ok(kind) => {
            state
                .settings
                .create_provider(scope, &ProviderConfig { id, kind })
                .await
        }
        Err(_) => Err(SettingsError::Invalid("Choose a provider type.".into())),
    };
    match result {
        Ok(()) => {
            apply(&state).await;
            Ok(Redirect::to(&format!("{}/providers", layer.base())).into_response())
        }
        Err(SettingsError::Invalid(message)) => {
            let page = render_providers(
                &state,
                &principal,
                &input.csrf,
                layer,
                ProvidersMessages {
                    new: Some(NewProvider {
                        id: input.id,
                        kind: input.kind,
                        base_url: input.base_url,
                    }),
                    error: Some(&message),
                    ..Default::default()
                },
            )
            .await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(SettingsError::Backend(error)) => Err(WebError::internal(error)),
    }
}

#[derive(Deserialize)]
struct UpdateProviderForm {
    csrf: String,
    #[serde(default)]
    base_url: String,
}

async fn provider_update(
    layer: Layer,
    Path(id): Path<String>,
    State(state): State<AppState>,
    jar: CookieJar,
    Form(input): Form<UpdateProviderForm>,
) -> Result<Response, WebError> {
    let Some((principal, scope)) = submitter(&state, &jar, &input.csrf, layer).await? else {
        return Ok(to_login());
    };
    match state
        .settings
        .update_provider(scope, &id, optional(&input.base_url))
        .await
    {
        Ok(true) => {
            apply(&state).await;
            Ok(Redirect::to(&format!("{}/providers", layer.base())).into_response())
        }
        Ok(false) => Err(WebError::not_found()),
        Err(SettingsError::Invalid(message)) => {
            let page = render_providers(
                &state,
                &principal,
                &input.csrf,
                layer,
                ProvidersMessages {
                    editing: Some(&id),
                    error: Some(&message),
                    ..Default::default()
                },
            )
            .await?;
            Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
        }
        Err(SettingsError::Backend(error)) => Err(WebError::internal(error)),
    }
}

#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
}

async fn provider_move(
    layer: Layer,
    Path(id): Path<String>,
    State(state): State<AppState>,
    jar: CookieJar,
    Form(input): Form<CsrfForm>,
) -> Result<Response, WebError> {
    let Some((_, scope)) = submitter(&state, &jar, &input.csrf, layer).await? else {
        return Ok(to_login());
    };
    if scope != Scope::Instance {
        return Err(WebError::not_found());
    }
    match state.settings.move_to_users(&id).await {
        Ok(Some(_)) => {
            state.keys.clear_cache().await;
            apply(&state).await;
            Ok(Redirect::to("/admin/providers").into_response())
        }
        Ok(None) => Err(WebError::not_found()),
        Err(error) => Err(WebError::internal(error)),
    }
}

async fn provider_delete(
    layer: Layer,
    Path(id): Path<String>,
    State(state): State<AppState>,
    jar: CookieJar,
    Form(input): Form<CsrfForm>,
) -> Result<Response, WebError> {
    let Some((_, scope)) = submitter(&state, &jar, &input.csrf, layer).await? else {
        return Ok(to_login());
    };
    match state.settings.delete_provider(scope, &id).await {
        Ok(true) => {
            state.keys.clear_cache().await;
            apply(&state).await;
            Ok(Redirect::to(&format!("{}/providers", layer.base())).into_response())
        }
        Ok(false) => Err(WebError::not_found()),
        Err(error) => Err(WebError::internal(error)),
    }
}

#[derive(Default)]
struct GuardrailsMessages<'a> {
    /// Unsaved values to show again after an error.
    config: Option<GuardrailsConfig>,
    /// The unsaved empty-row values to show again after an error.
    new_row: (&'a str, &'a str),
    error: Option<&'a str>,
    notice: Option<&'a str>,
    sample: Option<&'a str>,
    findings: Option<&'a [(String, String)]>,
}

async fn render_guardrails(
    state: &AppState,
    principal: &Principal,
    csrf: &str,
    layer: Layer,
    messages: GuardrailsMessages<'_>,
) -> Result<maud::Markup, WebError> {
    let scope = scope(layer, principal)?;
    let config = match messages.config {
        Some(config) => config,
        None => state
            .settings
            .guardrails(scope)
            .await
            .map_err(WebError::internal)?,
    };
    let instance = match layer {
        Layer::Mine => Some(
            state
                .settings
                .instance_guardrails()
                .await
                .map_err(WebError::internal)?,
        ),
        Layer::Instance => None,
    };
    Ok(views::settings::guardrails_page(&GuardrailsView {
        csrf,
        layer,
        superuser: is_superuser(principal),
        config: &config,
        enforced: instance.as_ref(),
        error: messages.error,
        notice: messages.notice,
        sample: messages.sample,
        findings: messages.findings,
        new_row: messages.new_row,
    }))
}

async fn guardrails_page(
    layer: Layer,
    State(state): State<AppState>,
    jar: CookieJar,
) -> Result<Response, WebError> {
    let Some((principal, csrf)) = account_principal(&state, &jar).await else {
        return Ok(to_login());
    };
    Ok(
        render_guardrails(&state, &principal, &csrf, layer, Default::default())
            .await?
            .into_response(),
    )
}

/// Which submit button of the guardrails form was used. Every button saves
/// the whole form; Add and Delete also change the pattern rows.
#[derive(Debug, PartialEq, Eq)]
enum PatternAction {
    Save,
    Add,
    Delete(usize),
}

struct GuardrailsForm {
    csrf: String,
    /// Everything except the pattern rows.
    config: GuardrailsConfig,
    /// Saved pattern rows, in page order, as submitted.
    rows: Vec<(String, String)>,
    /// The always-present empty row at the bottom.
    new_row: (String, String),
    action: PatternAction,
}

impl GuardrailsForm {
    /// The settings to store after applying the button, and the message to
    /// show once they are stored.
    fn resolve(&self) -> Result<(GuardrailsConfig, &'static str), String> {
        let mut config = self.config.clone();
        let mut detectors = BTreeMap::new();
        let rows = self
            .rows
            .iter()
            .enumerate()
            .filter(|(index, _)| self.action != PatternAction::Delete(*index))
            .map(|(_, row)| row);
        let new_row = match self.action {
            PatternAction::Add if self.new_row.0.is_empty() || self.new_row.1.is_empty() => {
                return Err("Fill in a name and a pattern to add.".into());
            }
            _ => Some(&self.new_row),
        };
        for (name, pattern) in rows.chain(new_row) {
            match (name.is_empty(), pattern.is_empty()) {
                (true, true) => {}
                (false, false) => {
                    if detectors.insert(name.clone(), pattern.clone()).is_some() {
                        return Err(format!("The pattern name \"{name}\" is used twice."));
                    }
                }
                (true, false) => return Err("A pattern is missing its name.".into()),
                (false, true) => return Err(format!("The pattern \"{name}\" is empty.")),
            }
        }
        config.regex.detectors = detectors;
        let notice = match self.action {
            PatternAction::Save => "Saved. New requests use these guardrails.",
            PatternAction::Add => {
                // A pattern added while custom patterns are off would never run.
                config.regex.enabled = true;
                "Pattern added. New requests use it."
            }
            PatternAction::Delete(_) => "Pattern removed.",
        };
        Ok((config, notice))
    }

    /// The submitted settings, to show again after an error.
    fn shown(&self) -> GuardrailsConfig {
        let mut config = self.config.clone();
        config.regex.detectors = self
            .rows
            .iter()
            .filter(|(name, _)| !name.is_empty())
            .cloned()
            .collect();
        config
    }
}

fn parse_guardrails_form(body: &[u8]) -> Result<GuardrailsForm, String> {
    let mut form = GuardrailsForm {
        csrf: String::new(),
        config: GuardrailsConfig {
            secrets: GuardrailConfig {
                enabled: false,
                detectors: Some(Vec::new()),
            },
            ..Default::default()
        },
        rows: Vec::new(),
        new_row: Default::default(),
        action: PatternAction::Save,
    };
    let mut all_secrets = false;
    let (mut names, mut patterns) = (Vec::new(), Vec::new());
    for (key, value) in url::form_urlencoded::parse(body) {
        let config = &mut form.config;
        match key.as_ref() {
            "csrf" => form.csrf = value.into_owned(),
            "enabled" => config.enabled = true,
            "mode" => config.mode = value.parse().map_err(|_| "Choose observe or mask.")?,
            "secrets_enabled" => config.secrets.enabled = true,
            "secrets_all" => all_secrets = true,
            "secret" => config
                .secrets
                .detectors
                .get_or_insert_with(Vec::new)
                .push(value.into_owned()),
            "regex_enabled" => config.regex.enabled = true,
            "regex_name" => names.push(value.trim().to_owned()),
            "regex_pattern" => patterns.push(value.trim().to_owned()),
            "new_regex_name" => form.new_row.0 = value.trim().to_owned(),
            "new_regex_pattern" => form.new_row.1 = value.trim().to_owned(),
            "op" => {
                form.action = match value.as_ref() {
                    "add" => PatternAction::Add,
                    other => other
                        .strip_prefix("delete:")
                        .and_then(|index| index.parse().ok())
                        .map_or(PatternAction::Save, PatternAction::Delete),
                }
            }
            _ => {}
        }
    }
    if all_secrets {
        form.config.secrets.detectors = None;
    }
    if names.len() != patterns.len() {
        return Err("Each pattern row needs a name and a pattern.".into());
    }
    form.rows = names.into_iter().zip(patterns).collect();
    Ok(form)
}

async fn guardrails_save(
    layer: Layer,
    State(state): State<AppState>,
    jar: CookieJar,
    RawForm(body): RawForm,
) -> Result<Response, WebError> {
    let csrf = url::form_urlencoded::parse(&body)
        .find(|(key, _)| key == "csrf")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    let Some((principal, scope)) = submitter(&state, &jar, &csrf, layer).await? else {
        return Ok(to_login());
    };
    let form = parse_guardrails_form(&body);
    let resolved = form
        .as_ref()
        .map_err(Clone::clone)
        .and_then(GuardrailsForm::resolve);
    let message = match resolved {
        Ok((config, notice)) => match state.settings.set_guardrails(scope, &config).await {
            Ok(()) => {
                apply(&state).await;
                let page = render_guardrails(
                    &state,
                    &principal,
                    &csrf,
                    layer,
                    GuardrailsMessages {
                        notice: Some(notice),
                        ..Default::default()
                    },
                )
                .await?;
                return Ok(page.into_response());
            }
            Err(SettingsError::Invalid(message)) => message,
            Err(SettingsError::Backend(error)) => return Err(WebError::internal(error)),
        },
        Err(message) => message,
    };
    let form = form.ok();
    let page = render_guardrails(
        &state,
        &principal,
        &csrf,
        layer,
        GuardrailsMessages {
            config: form.as_ref().map(GuardrailsForm::shown),
            new_row: form
                .as_ref()
                .map(|form| (form.new_row.0.as_str(), form.new_row.1.as_str()))
                .unwrap_or_default(),
            error: Some(&message),
            ..Default::default()
        },
    )
    .await?;
    Ok((StatusCode::UNPROCESSABLE_ENTITY, page).into_response())
}

#[derive(Deserialize)]
struct TestForm {
    csrf: String,
    #[serde(default)]
    sample: String,
}

/// What the saved guardrails of a layer find in a sample, with each match
/// shortened so the page does not repeat a whole secret.
fn findings(guardrails: &GuardrailsConfig, sample: &str) -> Result<Vec<(String, String)>, String> {
    let mut found = Vec::new();
    if !guardrails.enabled {
        return Ok(found);
    }
    let mut sets = Vec::new();
    if guardrails.secrets.enabled {
        sets.push(detect::DetectorSet::new(detect::select(
            &secrets::DETECTORS,
            guardrails.secrets.detectors.as_deref(),
        )));
    }
    if guardrails.regex.enabled {
        sets.push(
            detect::DetectorSet::regex(&guardrails.regex.detectors)
                .map_err(|error| format!("{error:#}"))?,
        );
    }
    for set in &sets {
        for finding in set.find(sample) {
            found.push((finding.detector.to_owned(), shorten(finding.secret)));
        }
    }
    Ok(found)
}

fn shorten(secret: &str) -> String {
    let characters: Vec<char> = secret.chars().collect();
    if characters.len() <= 8 {
        return secret.to_owned();
    }
    let head: String = characters[..4].iter().collect();
    let tail: String = characters[characters.len() - 2..].iter().collect();
    format!("{head}…{tail}")
}

async fn guardrails_test(
    layer: Layer,
    State(state): State<AppState>,
    jar: CookieJar,
    Form(input): Form<TestForm>,
) -> Result<Response, WebError> {
    let Some((principal, scope)) = submitter(&state, &jar, &input.csrf, layer).await? else {
        return Ok(to_login());
    };
    let instance = state
        .settings
        .instance_guardrails()
        .await
        .map_err(WebError::internal)?;
    let effective = match scope {
        Scope::Instance => effective_guardrails(&instance, None),
        Scope::User(user) => {
            let mine = state
                .settings
                .user_guardrails(user)
                .await
                .map_err(WebError::internal)?;
            effective_guardrails(&instance, Some(&mine))
        }
    };
    let (found, error) = match findings(&effective, &input.sample) {
        Ok(found) => (found, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    let notice = (!effective.enabled).then_some("Guardrails are off, so nothing is checked.");
    let mode = (effective.enabled && !found.is_empty()).then_some(match effective.mode {
        GuardrailsMode::Mask => "These would be masked before the request is sent.",
        GuardrailsMode::Observe => "These would be recorded; the request would be sent unchanged.",
    });
    let page = render_guardrails(
        &state,
        &principal,
        &input.csrf,
        layer,
        GuardrailsMessages {
            error: error.as_deref(),
            notice: notice.or(mode),
            sample: Some(&input.sample),
            findings: Some(&found),
            ..Default::default()
        },
    )
    .await?;
    Ok(page.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(body: &[u8]) -> Result<(GuardrailsConfig, &'static str), String> {
        parse_guardrails_form(body).unwrap().resolve()
    }

    #[test]
    fn the_guardrails_form_reads_every_setting() {
        let form = parse_guardrails_form(
            b"csrf=t&enabled=on&mode=mask&secrets_enabled=on&secret=github_token\
            &regex_enabled=on&regex_name=ticket&regex_pattern=TKT-%5Cd%2B&new_regex_name=&new_regex_pattern=",
        )
        .unwrap();
        assert_eq!(form.csrf, "t");
        assert_eq!(form.action, PatternAction::Save);
        let (config, _) = form.resolve().unwrap();
        assert!(config.enabled);
        assert_eq!(config.mode, GuardrailsMode::Mask);
        assert_eq!(
            config.secrets.detectors.as_deref(),
            Some(&["github_token".to_owned()][..])
        );
        assert_eq!(config.regex.detectors["ticket"], r"TKT-\d+");
        assert_eq!(config.regex.detectors.len(), 1);

        let (all, _) = resolve(b"csrf=t&secrets_all=on&secret=github_token").unwrap();
        assert!(all.secrets.detectors.is_none());
    }

    #[test]
    fn add_and_delete_change_only_their_row() {
        let rows = "regex_name=a&regex_pattern=x&regex_name=b&regex_pattern=y";

        let (added, notice) =
            resolve(format!("{rows}&new_regex_name=c&new_regex_pattern=z&op=add").as_bytes())
                .unwrap();
        assert_eq!(
            added.regex.detectors.keys().collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert!(added.regex.enabled, "adding turns custom patterns on");
        assert!(notice.starts_with("Pattern added"));

        let (deleted, _) = resolve(format!("{rows}&op=delete:0").as_bytes()).unwrap();
        assert_eq!(deleted.regex.detectors.keys().collect::<Vec<_>>(), ["b"]);

        assert!(resolve(format!("{rows}&new_regex_name=c&op=add").as_bytes()).is_err());
        assert!(resolve(format!("{rows}&op=add").as_bytes()).is_err());
        // Save keeps a filled bottom row too, so nothing typed is lost.
        let (saved, _) =
            resolve(format!("{rows}&new_regex_name=c&new_regex_pattern=z").as_bytes()).unwrap();
        assert_eq!(saved.regex.detectors.len(), 3);
        assert!(
            resolve(b"csrf=t&regex_name=a&regex_pattern=x&new_regex_name=a&new_regex_pattern=y")
                .is_err()
        );
        assert!(resolve(b"csrf=t&regex_name=&regex_pattern=x").is_err());
    }

    #[test]
    fn test_matches_are_shortened() {
        assert_eq!(shorten("short"), "short");
        assert_eq!(shorten("ghp_abcdefghijklmnop"), "ghp_…op");
    }
}
