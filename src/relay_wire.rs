//! The versioned interface shared by the server and AGS. Never includes upstream credentials.
use serde::{Deserialize, Serialize};

pub const SCOPE: &str = "relay:manage";
pub const RESOURCE_PATH: &str = "/api/relay";
pub const ACCESS_TOKEN_HEADER: &str = "x-aegis-access-token";
pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Provider {
    pub id: String,
    pub kind: String,
}

#[derive(Deserialize, Serialize)]
pub struct Connection {
    pub version: u32,
    pub providers: Vec<Provider>,
}

#[derive(Deserialize, Serialize)]
pub struct Enrollment {
    pub providers: Vec<String>,
}
