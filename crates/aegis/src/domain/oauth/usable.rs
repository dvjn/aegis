//! Whether a stored token can still be used, decided once for every caller.
use chrono::{DateTime, TimeDelta, Utc};
use sea_orm::{ColumnTrait, ConnectionTrait, EntityTrait, QueryFilter};
use std::collections::HashMap;

use super::{GATEWAY_SCOPE, OAuthService, parse_stored_date, scope_allows};
use crate::domain::{
    entity::{oauth_access_tokens, oauth_refresh_token_families, oauth_refresh_tokens, users},
    error::DomainError,
};

/// What a token is presented for, at `now`. `None` accepts any resource or scope.
pub(super) struct TokenUse<'a> {
    pub key_id: &'a str,
    pub issuer: &'a str,
    pub resource: Option<&'a str>,
    pub scope: Option<&'a str>,
    pub now: DateTime<Utc>,
}

impl TokenUse<'_> {
    fn binds(&self, issuer: &str, resource: &str, scope: &str) -> bool {
        issuer == self.issuer
            && self.resource.is_none_or(|wanted| wanted == resource)
            && self.scope.is_none_or(|wanted| scope_allows(scope, wanted))
    }

    fn before(&self, stored: &str) -> bool {
        parse_stored_date(stored).is_ok_and(|at| self.now < at)
    }
}

fn user_signs_in(user: &users::Model, auth_version: i64) -> bool {
    user.status == "active" && user.auth_version == auth_version
}

pub(super) fn access_token_usable(
    token_use: &TokenUse,
    token: &oauth_access_tokens::Model,
    family: Option<&oauth_refresh_token_families::Model>,
    user: &users::Model,
) -> bool {
    token.key_id == token_use.key_id
        && token_use.binds(&token.issuer, &token.resource, &token.scope)
        && token.revoked_at.is_none()
        && family.is_none_or(|family| family.revoked_at.is_none())
        && user_signs_in(user, token.auth_version)
        && token_use.before(&token.expires_at)
}

/// A presented refresh token not meant for this use is refused without counting as reuse.
pub(super) fn refresh_token_bound(
    token_use: &TokenUse,
    token: &oauth_refresh_tokens::Model,
    family: &oauth_refresh_token_families::Model,
) -> bool {
    token.key_id == token_use.key_id
        && token_use.binds(&family.issuer, &family.resource, &family.scope)
}

pub(super) fn refresh_family_usable(
    token_use: &TokenUse,
    family: &oauth_refresh_token_families::Model,
    user: &users::Model,
) -> bool {
    token_use.binds(&family.issuer, &family.resource, &family.scope)
        && family.revoked_at.is_none()
        && user_signs_in(user, family.auth_version)
        && token_use.before(&family.absolute_expires_at)
}

pub(super) fn refresh_token_usable(
    token_use: &TokenUse,
    token: &oauth_refresh_tokens::Model,
    family: &oauth_refresh_token_families::Model,
    user: &users::Model,
) -> bool {
    refresh_token_bound(token_use, token, family)
        && token.rotated_at.is_none()
        && token.revoked_at.is_none()
        && token_use.before(&token.idle_expires_at)
        && refresh_family_usable(token_use, family, user)
}

/// The tokens of some users that may still be usable, to check many grants at once.
pub(super) struct Grants {
    users: HashMap<String, users::Model>,
    families: HashMap<String, oauth_refresh_token_families::Model>,
    refresh_tokens: Vec<oauth_refresh_tokens::Model>,
    access_tokens: Vec<oauth_access_tokens::Model>,
}

impl Grants {
    /// The families of `user_id` that still hold a usable refresh token.
    pub(super) fn usable_families<'a>(
        &'a self,
        user_id: &'a str,
        token_use: &'a TokenUse,
    ) -> impl Iterator<Item = &'a oauth_refresh_token_families::Model> {
        let user = self.users.get(user_id);
        self.families.values().filter(move |family| {
            family.user_id == user_id
                && user.is_some_and(|user| {
                    self.refresh_tokens.iter().any(|token| {
                        token.family_id == family.id
                            && refresh_token_usable(token_use, token, family, user)
                    })
                })
        })
    }

    /// The usable access tokens of `user_id`.
    pub(super) fn usable_access_tokens<'a>(
        &'a self,
        user_id: &'a str,
        token_use: &'a TokenUse,
    ) -> impl Iterator<Item = &'a oauth_access_tokens::Model> {
        let user = self.users.get(user_id);
        self.access_tokens.iter().filter(move |token| {
            let family = token.family_id.as_ref().map(|id| self.families.get(id));
            token.user_id == user_id
                // Only unrevoked families are loaded.
                && family.is_none_or(|family| family.is_some())
                && user.is_some_and(|user| {
                    access_token_usable(token_use, token, family.flatten(), user)
                })
        })
    }

    /// Whether `user_id` can still use a refresh or access token of `client_id`.
    pub(super) fn usable(&self, user_id: &str, client_id: &str, token_use: &TokenUse) -> bool {
        self.usable_families(user_id, token_use)
            .any(|family| family.client_id == client_id)
            || self
                .usable_access_tokens(user_id, token_use)
                .any(|token| token.client_id == client_id)
    }
}

impl OAuthService {
    /// The grants of `user_ids`, for `client_ids` when given.
    pub(super) async fn load_grants(
        &self,
        db: &impl ConnectionTrait,
        user_ids: Vec<String>,
        client_ids: Option<Vec<String>>,
        now: DateTime<Utc>,
    ) -> Result<Grants, DomainError> {
        let users = users::Entity::find()
            .filter(users::Column::Id.is_in(user_ids.clone()))
            .all(db)
            .await?
            .into_iter()
            .map(|user| (user.id.clone(), user))
            .collect();
        let mut families = oauth_refresh_token_families::Entity::find()
            .filter(oauth_refresh_token_families::Column::UserId.is_in(user_ids.clone()))
            .filter(oauth_refresh_token_families::Column::RevokedAt.is_null());
        let mut access_tokens = oauth_access_tokens::Entity::find()
            .filter(oauth_access_tokens::Column::UserId.is_in(user_ids))
            .filter(oauth_access_tokens::Column::RevokedAt.is_null())
            // Only narrows the load: expired tokens pile up and none of them is usable.
            .filter(
                oauth_access_tokens::Column::ExpiresAt.gt((now - TimeDelta::days(1)).to_rfc3339()),
            );
        if let Some(client_ids) = client_ids {
            families = families
                .filter(oauth_refresh_token_families::Column::ClientId.is_in(client_ids.clone()));
            access_tokens =
                access_tokens.filter(oauth_access_tokens::Column::ClientId.is_in(client_ids));
        }
        let families: HashMap<_, _> = families
            .all(db)
            .await?
            .into_iter()
            .map(|family| (family.id.clone(), family))
            .collect();
        let refresh_tokens = oauth_refresh_tokens::Entity::find()
            .filter(oauth_refresh_tokens::Column::FamilyId.is_in(families.keys().cloned()))
            .filter(oauth_refresh_tokens::Column::RotatedAt.is_null())
            .filter(oauth_refresh_tokens::Column::RevokedAt.is_null())
            .all(db)
            .await?;
        Ok(Grants {
            users,
            families,
            refresh_tokens,
            access_tokens: access_tokens.all(db).await?,
        })
    }

    pub(super) fn token_use<'a>(
        &'a self,
        issuer: &'a str,
        resource: Option<&'a str>,
        now: DateTime<Utc>,
    ) -> TokenUse<'a> {
        TokenUse {
            key_id: &self.config.key_id,
            issuer,
            resource,
            scope: None,
            now,
        }
    }

    /// A paired machine's use of the gateway, whose resource for `issuer` is `resource`.
    pub(super) fn machine_use<'a>(
        &'a self,
        issuer: &'a str,
        resource: &'a str,
        now: DateTime<Utc>,
    ) -> TokenUse<'a> {
        TokenUse {
            resource: Some(resource),
            scope: Some(GATEWAY_SCOPE),
            ..self.token_use(issuer, None, now)
        }
    }
}
