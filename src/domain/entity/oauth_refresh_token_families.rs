use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "oauth_refresh_token_families")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub user_id: String,
    pub client_id: String,
    pub issuer: String,
    pub resource: String,
    pub scope: String,
    pub auth_version: i64,
    pub created_at: String,
    pub absolute_expires_at: String,
    pub revoked_at: Option<String>,
    pub revocation_reason: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}
