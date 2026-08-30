use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "users")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub email_normalized: String,
    pub email_display: String,
    pub role: String,
    pub status: String,
    pub auth_version: i64,
    pub email_verified_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl Related<super::password_credentials::Entity> for Entity {
    fn to() -> RelationDef {
        super::password_credentials::Relation::Users.def().rev()
    }
}
impl ActiveModelBehavior for ActiveModel {}
