//! Provider grants and reporting identities for OAuth-connected relays. No API keys.
use super::OAuthService;
use crate::ags_wire::{AccountRegistration, ProfileLabel, VENDORS};
use crate::domain::{DomainError, Principal};
use sea_orm::{ConnectionTrait, DbBackend, Statement};
use uuid::Uuid;

impl OAuthService {
    pub async fn enroll_relay(
        &self,
        principal: &Principal,
        providers: &[String],
    ) -> Result<(), DomainError> {
        let oauth = principal.oauth().ok_or(DomainError::InvalidCredentials)?;
        let client = self.find_client(&oauth.client_id, &oauth.issuer).await?;
        let mut providers = providers.to_vec();
        providers.sort();
        providers.dedup();
        if providers.is_empty() {
            return Err(DomainError::InvalidInput("no providers selected"));
        }
        let allowed = serde_json::to_string(&providers)
            .map_err(|_| DomainError::InvalidInput("invalid providers"))?;
        let tx = self.begin_immediate().await?;
        tx.execute_raw(Statement::from_sql_and_values(DbBackend::Sqlite,
            "INSERT INTO gateway_clients(id,user_id,kind,name,oauth_client_id,oauth_issuer,allowed_providers,created_at)
             VALUES(?,?,'oauth',?,?,?,?,?) ON CONFLICT(user_id,oauth_client_id,oauth_issuer)
             DO UPDATE SET name=excluded.name,allowed_providers=excluded.allowed_providers",
            vec![Uuid::now_v7().to_string().into(),principal.user_id().to_string().into(),client.client_name().unwrap_or("Connected app").to_owned().into(),
                oauth.client_id.clone().into(),oauth.issuer.clone().into(),allowed.into(),self.clock.now().to_rfc3339().into()])).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn relay_client(
        &self,
        principal: &Principal,
    ) -> Result<(String, Vec<String>), DomainError> {
        let oauth = principal.oauth().ok_or(DomainError::InvalidCredentials)?;
        let row = self.db.query_one_raw(Statement::from_sql_and_values(DbBackend::Sqlite,
            "SELECT id,allowed_providers FROM gateway_clients WHERE user_id=? AND kind='oauth' AND oauth_client_id=? AND oauth_issuer=?",
            [principal.user_id().to_string().into(),oauth.client_id.clone().into(),oauth.issuer.clone().into()])).await?
            .ok_or(DomainError::InvalidCredentials)?;
        let allowed: String = row.try_get("", "allowed_providers")?;
        let providers =
            serde_json::from_str(&allowed).map_err(|_| DomainError::InvalidCredentials)?;
        Ok((row.try_get("", "id")?, providers))
    }

    /// Called only after token audience, expiry, scope and revocation were checked.
    /// An account must be this client's live account for this route; a profile must be this
    /// client's live profile. Returns the client ID.
    pub async fn relay_identity(
        &self,
        principal: &Principal,
        provider: &str,
        account: Option<&str>,
        profile: Option<&str>,
    ) -> Result<String, DomainError> {
        let (id, providers) = self.relay_client(principal).await?;
        if !providers.iter().any(|id| id == provider) {
            return Err(DomainError::InvalidInput("provider not granted"));
        }
        if let Some(account_id) = account {
            let row = self.db.query_one_raw(Statement::from_sql_and_values(DbBackend::Sqlite,
                "SELECT provider FROM relay_accounts WHERE id=? AND client_id=? AND deleted_at IS NULL",
                [account_id.into(), id.clone().into()])).await?
                .ok_or(DomainError::Forbidden)?;
            let account_provider: String = row.try_get("", "provider")?;
            if account_provider != provider {
                return Err(DomainError::Forbidden);
            }
        }
        if let Some(profile_id) = profile {
            self.db.query_one_raw(Statement::from_sql_and_values(DbBackend::Sqlite,
                "SELECT 1 FROM relay_profiles WHERE id=? AND client_id=? AND deleted_at IS NULL",
                [profile_id.into(), id.clone().into()])).await?
                .ok_or(DomainError::Forbidden)?;
        }
        crate::db::writer(&self.db)
            .await?
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "UPDATE gateway_clients SET last_used_at=? WHERE id=?",
                [self.clock.now().to_rfc3339().into(), id.clone().into()],
            ))
            .await?;
        Ok(id)
    }

    /// Registers or updates an account of the calling client. An ID owned by another client
    /// is never touched. `provider_kind` is the gateway's kind for `account.provider`
    /// (`None` when the gateway has no such provider).
    pub async fn register_account(
        &self,
        principal: &Principal,
        id: &str,
        account: &AccountRegistration,
        provider_kind: Option<&str>,
    ) -> Result<(), DomainError> {
        if Uuid::parse_str(id).is_err() {
            return Err(DomainError::InvalidInput("invalid account id"));
        }
        let Some((_, kind)) = VENDORS.iter().find(|(vendor, _)| *vendor == account.vendor) else {
            return Err(DomainError::InvalidInput("invalid vendor"));
        };
        if provider_kind != Some(*kind) {
            return Err(DomainError::InvalidInput("provider does not serve vendor"));
        }
        let label = valid_label(&account.label)?;
        let (client, providers) = self.relay_client(principal).await?;
        if !providers.contains(&account.provider) {
            return Err(DomainError::InvalidInput("provider not granted"));
        }
        let tx = self.begin_immediate().await?;
        if let Some(row) = tx
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT client_id,vendor FROM relay_accounts WHERE id=?",
                [id.into()],
            ))
            .await?
        {
            let owner: String = row.try_get("", "client_id")?;
            let vendor: String = row.try_get("", "vendor")?;
            if owner != client {
                return Err(DomainError::Forbidden);
            }
            if vendor != account.vendor {
                return Err(DomainError::InvalidInput("vendor cannot change"));
            }
        }
        let now = self.clock.now().to_rfc3339();
        tx.execute_raw(Statement::from_sql_and_values(DbBackend::Sqlite,
            "INSERT INTO relay_accounts(id,client_id,vendor,label,provider,created_at,updated_at) VALUES(?,?,?,?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET label=excluded.label,provider=excluded.provider,updated_at=excluded.updated_at,deleted_at=NULL",
            [id.into(),client.into(),account.vendor.clone().into(),label.into(),account.provider.clone().into(),now.clone().into(),now.into()])).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_account(&self, principal: &Principal, id: &str) -> Result<(), DomainError> {
        self.soft_delete("relay_accounts", principal, id).await
    }

    /// Registers or renames a profile of the calling client.
    pub async fn register_profile(
        &self,
        principal: &Principal,
        id: &str,
        profile: &ProfileLabel,
    ) -> Result<(), DomainError> {
        if Uuid::parse_str(id).is_err() {
            return Err(DomainError::InvalidInput("invalid profile id"));
        }
        let label = valid_label(&profile.label)?;
        let (client, _) = self.relay_client(principal).await?;
        let tx = self.begin_immediate().await?;
        if let Some(row) = tx
            .query_one_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT client_id FROM relay_profiles WHERE id=?",
                [id.into()],
            ))
            .await?
        {
            let owner: String = row.try_get("", "client_id")?;
            if owner != client {
                return Err(DomainError::Forbidden);
            }
        }
        let now = self.clock.now().to_rfc3339();
        tx.execute_raw(Statement::from_sql_and_values(DbBackend::Sqlite,
            "INSERT INTO relay_profiles(id,client_id,label,created_at,updated_at) VALUES(?,?,?,?,?)
             ON CONFLICT(id) DO UPDATE SET label=excluded.label,updated_at=excluded.updated_at,deleted_at=NULL",
            [id.into(),client.into(),label.into(),now.clone().into(),now.into()])).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn delete_profile(&self, principal: &Principal, id: &str) -> Result<(), DomainError> {
        self.soft_delete("relay_profiles", principal, id).await
    }

    async fn soft_delete(
        &self,
        table: &str,
        principal: &Principal,
        id: &str,
    ) -> Result<(), DomainError> {
        let (client, _) = self.relay_client(principal).await?;
        let now = self.clock.now().to_rfc3339();
        let done = crate::db::writer(&self.db)
            .await?
            .execute_raw(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                format!("UPDATE {table} SET deleted_at=?,updated_at=? WHERE id=? AND client_id=? AND deleted_at IS NULL"),
                [now.clone().into(), now.into(), id.into(), client.into()],
            ))
            .await?;
        if done.rows_affected() == 0 {
            return Err(DomainError::NotFound);
        }
        Ok(())
    }
}

fn valid_label(label: &str) -> Result<String, DomainError> {
    let label = label.trim();
    if label.is_empty() || label.chars().count() > 64 {
        return Err(DomainError::InvalidInput("invalid label"));
    }
    Ok(label.to_owned())
}
