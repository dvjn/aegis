use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "oauth_device_authorizations")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub device_code_digest: Vec<u8>,
    pub user_code_digest: Vec<u8>,
    pub key_id: String,
    pub client_id: String,
    pub issuer: String,
    pub resource: String,
    pub scope: String,
    pub status: String,
    pub user_id: Option<String>,
    pub auth_version: Option<i64>,
    pub created_at: String,
    pub expires_at: String,
    pub interval_seconds: i64,
    pub last_poll_at: Option<String>,
    pub decision_at: Option<String>,
    pub consumed_at: Option<String>,
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
