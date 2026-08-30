use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "oauth_authorization_codes")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub secret_digest: Vec<u8>,
    pub key_id: String,
    pub user_id: String,
    pub client_id: String,
    pub issuer: String,
    pub redirect_uri: String,
    pub registered_redirect_uri: String,
    pub resource: String,
    pub scope: String,
    pub auth_version: i64,
    pub code_challenge: String,
    pub code_challenge_method: String,
    pub created_at: String,
    pub expires_at: String,
    pub consumed_at: Option<String>,
    pub issued_access_token_id: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::users::Entity",
        from = "Column::UserId",
        to = "super::users::Column::Id"
    )]
    Users,
}
impl Related<super::users::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Users.def()
    }
}
impl ActiveModelBehavior for ActiveModel {}
