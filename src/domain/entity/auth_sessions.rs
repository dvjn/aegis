use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "auth_sessions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub user_id: String,
    pub transport: String,
    pub secret_digest: Vec<u8>,
    pub csrf_digest: Option<Vec<u8>>,
    pub key_id: String,
    pub auth_version: i64,
    pub authenticated_at: String,
    pub created_at: String,
    pub last_seen_at: String,
    pub idle_expires_at: String,
    pub absolute_expires_at: String,
    pub revoked_at: Option<String>,
    pub revocation_reason: Option<String>,
    pub user_agent: Option<String>,
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
