use std::collections::HashSet;

use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DbBackend, EntityTrait, QueryFilter, Set,
    Statement,
};
use url::Url;
use uuid::Uuid;

use super::{
    DEVICE_GRANT_TYPE, OAuthService, is_loopback_redirect, is_private_use_redirect,
    redirect_uri_matches, scope_allows, scope_is_subset, validate_client_id, validate_issuer,
    validate_redirect_uri, validate_scope,
};
use crate::domain::{
    entity::{oauth_client_redirect_uris, oauth_clients},
    error::DomainError,
};

const MAX_REDIRECT_URIS: usize = 16;
const MAX_REGISTRATIONS_PER_ISSUER: i64 = 10_000;
const MAX_REGISTRATION_METADATA_BYTES: usize = 8 * 1024;

pub struct ClientRegistration {
    pub issuer: String,
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
    pub application_type: Option<String>,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub scope: String,
}

impl ClientRegistration {
    fn application_type(&self) -> &str {
        self.application_type
            .as_deref()
            .unwrap_or_else(|| infer_application_type(&self.redirect_uris))
    }
}

fn infer_application_type(redirect_uris: &[String]) -> &'static str {
    let native = !redirect_uris.is_empty()
        && redirect_uris.iter().all(|value| {
            Url::parse(value)
                .is_ok_and(|url| is_loopback_redirect(&url) || is_private_use_redirect(&url))
        });
    if native { "native" } else { "web" }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredClient {
    pub(super) client_id: String,
    pub(super) issuer: String,
    pub(super) redirect_uris: Vec<String>,
    pub(super) client_name: Option<String>,
    pub(super) application_type: String,
    pub(super) grant_types: Vec<String>,
    pub(super) response_types: Vec<String>,
    pub(super) scope: String,
}
impl RegisteredClient {
    pub fn client_id(&self) -> &str {
        &self.client_id
    }
    pub fn redirect_uris(&self) -> &[String] {
        &self.redirect_uris
    }
    pub fn client_name(&self) -> Option<&str> {
        self.client_name.as_deref()
    }
    pub fn token_endpoint_auth_method(&self) -> &'static str {
        "none"
    }
    pub fn application_type(&self) -> &str {
        &self.application_type
    }
    pub fn grant_types(&self) -> &[String] {
        &self.grant_types
    }
    pub fn response_types(&self) -> &[String] {
        &self.response_types
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    pub fn allows_scope(&self, requested: &str) -> bool {
        scope_is_subset(requested, &self.scope)
    }
    pub fn allows_grant(&self, grant_type: &str) -> bool {
        self.grant_types.iter().any(|value| value == grant_type)
    }
}

impl OAuthService {
    pub async fn register_public_client(
        &self,
        registration: ClientRegistration,
    ) -> Result<RegisteredClient, DomainError> {
        validate_registration(&registration)?;
        let application_type = registration.application_type().to_owned();
        let client_id = Uuid::now_v7().to_string();
        let now = self.clock.now().to_rfc3339();
        let grant_types = registration.grant_types.join(" ");
        let response_types = registration.response_types.join(" ");
        let tx = crate::db::begin_immediate(&self.db).await?;
        let inserted = tx
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "INSERT INTO oauth_clients(id,issuer,client_name,application_type,grant_types,response_types,scope,token_endpoint_auth_method,created_at) SELECT ?,?,?,?,?,?,?,'none',? WHERE (SELECT count(*) FROM oauth_clients WHERE issuer=?) < ?",
                vec![
                    client_id.clone().into(),
                    registration.issuer.clone().into(),
                    registration.client_name.clone().into(),
                    application_type.clone().into(),
                    grant_types.into(),
                    response_types.into(),
                    registration.scope.clone().into(),
                    now.into(),
                    registration.issuer.clone().into(),
                    MAX_REGISTRATIONS_PER_ISSUER.into(),
                ],
            ))
            .await?;
        if inserted.rows_affected() != 1 {
            return Err(DomainError::InvalidInput(
                "client registration limit reached",
            ));
        }
        for redirect_uri in &registration.redirect_uris {
            oauth_client_redirect_uris::ActiveModel {
                client_id: Set(client_id.clone()),
                issuer: Set(registration.issuer.clone()),
                redirect_uri: Set(redirect_uri.clone()),
            }
            .insert(&tx)
            .await?;
        }
        tx.commit().await?;
        Ok(RegisteredClient {
            client_id,
            issuer: registration.issuer,
            redirect_uris: registration.redirect_uris,
            client_name: registration.client_name,
            application_type,
            grant_types: registration.grant_types,
            response_types: registration.response_types,
            scope: registration.scope,
        })
    }

    pub async fn validate_authorization_client(
        &self,
        client_id: &str,
        issuer: &str,
        redirect_uri: &str,
    ) -> Result<RegisteredClient, DomainError> {
        self.find_authorization_client(client_id, issuer, redirect_uri)
            .await
            .map(|(client, _)| client)
    }

    pub(super) async fn find_authorization_client(
        &self,
        client_id: &str,
        issuer: &str,
        redirect_uri: &str,
    ) -> Result<(RegisteredClient, String), DomainError> {
        validate_client_id(client_id).map_err(|_| DomainError::InvalidCredentials)?;
        validate_issuer(issuer).map_err(|_| DomainError::InvalidCredentials)?;
        validate_redirect_uri(redirect_uri).map_err(|_| DomainError::InvalidCredentials)?;
        let client = self.find_client(client_id, issuer).await?;
        if !client.allows_grant("authorization_code") {
            return Err(DomainError::InvalidCredentials);
        }
        let uris = oauth_client_redirect_uris::Entity::find()
            .filter(oauth_client_redirect_uris::Column::ClientId.eq(client_id))
            .filter(oauth_client_redirect_uris::Column::Issuer.eq(issuer))
            .all(&self.db)
            .await?;
        let registered_redirect_uri = uris
            .iter()
            .find_map(|row| {
                redirect_uri_matches(&row.redirect_uri, redirect_uri)
                    .then(|| row.redirect_uri.clone())
            })
            .ok_or(DomainError::InvalidCredentials)?;
        Ok((
            RegisteredClient {
                redirect_uris: uris.into_iter().map(|row| row.redirect_uri).collect(),
                ..client
            },
            registered_redirect_uri,
        ))
    }

    pub async fn find_client(
        &self,
        client_id: &str,
        issuer: &str,
    ) -> Result<RegisteredClient, DomainError> {
        validate_client_id(client_id).map_err(|_| DomainError::InvalidCredentials)?;
        validate_issuer(issuer).map_err(|_| DomainError::InvalidCredentials)?;
        let client = oauth_clients::Entity::find_by_id(client_id)
            .filter(oauth_clients::Column::Issuer.eq(issuer))
            .one(&self.db)
            .await?
            .ok_or(DomainError::InvalidCredentials)?;
        let redirect_uris = oauth_client_redirect_uris::Entity::find()
            .filter(oauth_client_redirect_uris::Column::ClientId.eq(client_id))
            .filter(oauth_client_redirect_uris::Column::Issuer.eq(issuer))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| row.redirect_uri)
            .collect();
        let words = |value: String| {
            if value.is_empty() {
                Vec::new()
            } else {
                value.split(' ').map(str::to_owned).collect()
            }
        };
        Ok(RegisteredClient {
            client_id: client_id.to_owned(),
            issuer: issuer.to_owned(),
            redirect_uris,
            client_name: client.client_name,
            application_type: client.application_type,
            grant_types: words(client.grant_types),
            response_types: words(client.response_types),
            scope: client.scope,
        })
    }
}

fn validate_registration(registration: &ClientRegistration) -> Result<(), DomainError> {
    validate_issuer(&registration.issuer)?;
    if registration.redirect_uris.len() > MAX_REDIRECT_URIS
        || registration
            .redirect_uris
            .iter()
            .map(String::len)
            .sum::<usize>()
            + registration.client_name.as_ref().map_or(0, String::len)
            + registration.scope.len()
            + registration
                .grant_types
                .iter()
                .map(String::len)
                .sum::<usize>()
            + registration
                .response_types
                .iter()
                .map(String::len)
                .sum::<usize>()
            > MAX_REGISTRATION_METADATA_BYTES
    {
        return Err(DomainError::InvalidInput("invalid redirect URI metadata"));
    }
    if registration
        .client_name
        .as_ref()
        .is_some_and(|name| name.is_empty() || name.len() > 128 || name.contains(char::is_control))
    {
        return Err(DomainError::InvalidInput("invalid client metadata"));
    }
    let authorization_code = registration
        .grant_types
        .iter()
        .any(|value| value == "authorization_code");
    let device_code = registration
        .grant_types
        .iter()
        .any(|value| value == DEVICE_GRANT_TYPE);
    let grants_valid = !registration.grant_types.is_empty()
        && registration.grant_types.len() <= 3
        && registration.grant_types.iter().all(|value| {
            matches!(value.as_str(), "authorization_code" | "refresh_token")
                || value == DEVICE_GRANT_TYPE
        })
        && registration
            .grant_types
            .iter()
            .collect::<HashSet<_>>()
            .len()
            == registration.grant_types.len()
        && (authorization_code || device_code);
    let responses_valid = if authorization_code {
        supported_values(&registration.response_types, &["code"], "code")
    } else {
        registration.response_types.is_empty()
    };
    if !matches!(registration.application_type(), "native" | "web")
        || !grants_valid
        || !responses_valid
        || (authorization_code && registration.redirect_uris.is_empty())
        || (!authorization_code && !registration.redirect_uris.is_empty())
        || validate_scope(&registration.scope).is_err()
        || (scope_allows(&registration.scope, "offline_access")
            && !registration
                .grant_types
                .iter()
                .any(|value| value == "refresh_token"))
    {
        return Err(DomainError::InvalidInput("invalid client metadata"));
    }
    let mut unique = HashSet::with_capacity(registration.redirect_uris.len());
    for redirect_uri in &registration.redirect_uris {
        validate_redirect_uri(redirect_uri)?;
        let url = Url::parse(redirect_uri)
            .map_err(|_| DomainError::InvalidInput("invalid redirect URI"))?;
        let valid_for_application = match registration.application_type() {
            "web" => url.scheme() == "https" && url.host_str().is_some(),
            "native" => is_loopback_redirect(&url) || is_private_use_redirect(&url),
            _ => false,
        };
        if !valid_for_application || !unique.insert(redirect_uri.as_str()) {
            return Err(DomainError::InvalidInput("invalid redirect URI"));
        }
    }
    Ok(())
}

fn supported_values(values: &[String], supported: &[&str], required: &str) -> bool {
    !values.is_empty()
        && values.len() <= supported.len()
        && values.iter().any(|value| value == required)
        && values
            .iter()
            .all(|value| supported.contains(&value.as_str()))
        && values.iter().collect::<HashSet<_>>().len() == values.len()
}

#[cfg(test)]
mod tests {
    use super::super::tests::setup;
    use super::*;
    use sea_orm::QueryResult;

    #[tokio::test]
    async fn registration_is_public_issuer_bound_and_exact() {
        let (db, oauth, _, _, _, _) = setup().await;
        assert!(
            oauth
                .register_public_client(ClientRegistration {
                    issuer: "https://aegis.example".into(),
                    redirect_uris: vec![
                        "https://client.example/cb".into(),
                        "https://client.example/cb".into()
                    ],
                    client_name: None,
                    application_type: Some("web".into()),
                    grant_types: vec!["authorization_code".into()],
                    response_types: vec!["code".into()],
                    scope: "usage:read".into(),
                })
                .await
                .is_err()
        );
        assert!(
            oauth
                .register_public_client(ClientRegistration {
                    issuer: "https://aegis.example".into(),
                    redirect_uris: vec!["https://client.example/cb#fragment".into()],
                    client_name: None,
                    application_type: Some("web".into()),
                    grant_types: vec!["authorization_code".into()],
                    response_types: vec!["code".into()],
                    scope: "usage:read".into(),
                })
                .await
                .is_err()
        );
        let row: QueryResult = db
            .query_one_raw(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT token_endpoint_auth_method FROM oauth_clients LIMIT 1",
            ))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            row.try_get::<String>("", "token_endpoint_auth_method")
                .unwrap(),
            "none"
        );
    }

    #[test]
    fn application_type_is_inferred_from_redirect_uris() {
        assert_eq!(
            infer_application_type(&["https://chatgpt.example/callback".into()]),
            "web"
        );
        assert_eq!(
            infer_application_type(&["http://127.0.0.1:49152/callback".into()]),
            "native"
        );
        assert_eq!(
            infer_application_type(&["com.example.client:/callback".into()]),
            "native"
        );
        assert_eq!(
            infer_application_type(&[
                "http://127.0.0.1:49152/callback".into(),
                "https://chatgpt.example/callback".into()
            ]),
            "web"
        );
        assert_eq!(infer_application_type(&[]), "web");
    }

    #[tokio::test]
    async fn hosted_client_registers_without_application_type() {
        let (_, oauth, _, _, _, _) = setup().await;
        let client = oauth
            .register_public_client(ClientRegistration {
                issuer: "https://aegis.example".into(),
                redirect_uris: vec!["https://chatgpt.example/callback".into()],
                client_name: Some("ChatGPT".into()),
                application_type: None,
                grant_types: vec!["authorization_code".into()],
                response_types: vec!["code".into()],
                scope: "usage:read".into(),
            })
            .await
            .unwrap();
        assert_eq!(client.application_type(), "web");
    }

    #[tokio::test]
    async fn native_client_registers_without_application_type() {
        let (_, oauth, _, _, _, _) = setup().await;
        let client = oauth
            .register_public_client(ClientRegistration {
                issuer: "https://aegis.example".into(),
                redirect_uris: vec!["http://127.0.0.1:49152/callback".into()],
                client_name: None,
                application_type: None,
                grant_types: vec!["authorization_code".into()],
                response_types: vec!["code".into()],
                scope: "usage:read".into(),
            })
            .await
            .unwrap();
        assert_eq!(client.application_type(), "native");
    }
}
