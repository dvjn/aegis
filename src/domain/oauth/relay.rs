//! Provider grants and reporting identities for OAuth-connected relays. No API keys.
use super::OAuthService;
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

    /// Called only after token audience, expiry, scope and revocation were checked.
    pub async fn relay_identity(
        &self,
        principal: &Principal,
        provider: &str,
    ) -> Result<String, DomainError> {
        let oauth = principal.oauth().ok_or(DomainError::InvalidCredentials)?;
        let row = self.db.query_one_raw(Statement::from_sql_and_values(DbBackend::Sqlite,
            "SELECT id,allowed_providers FROM gateway_clients WHERE user_id=? AND kind='oauth' AND oauth_client_id=? AND oauth_issuer=?",
            [principal.user_id().to_string().into(),oauth.client_id.clone().into(),oauth.issuer.clone().into()])).await?
            .ok_or(DomainError::InvalidCredentials)?;
        let allowed: String = row.try_get("", "allowed_providers")?;
        let providers: Vec<String> =
            serde_json::from_str(&allowed).map_err(|_| DomainError::InvalidCredentials)?;
        if !providers.iter().any(|id| id == provider) {
            return Err(DomainError::InvalidInput("provider not granted"));
        }
        let id: String = row.try_get("", "id")?;
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
}
