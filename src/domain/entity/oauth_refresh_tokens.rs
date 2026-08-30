use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "oauth_refresh_tokens")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub family_id: String,
    pub secret_digest: Vec<u8>,
    pub key_id: String,
    pub generation: i64,
    pub created_at: String,
    pub idle_expires_at: String,
    pub rotated_at: Option<String>,
    pub revoked_at: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::oauth_refresh_token_families::Entity",
        from = "Column::FamilyId",
        to = "super::oauth_refresh_token_families::Column::Id"
    )]
    Family,
}
impl Related<super::oauth_refresh_token_families::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Family.def()
    }
}
impl ActiveModelBehavior for ActiveModel {}
