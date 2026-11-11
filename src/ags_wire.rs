//! The versioned interface shared by the server and AGS. Never includes upstream credentials.
use serde::{Deserialize, Serialize};

pub const SCOPE: &str = "relay:manage";
pub const RESOURCE_PATH: &str = "/api/relay";
pub const ACCESS_TOKEN_HEADER: &str = "x-aegis-access-token";
pub const PROTOCOL_VERSION: u32 = 1;
/// Internal attribution headers set by the AGS relay; the server strips them before upstream.
pub const PROFILE_HEADER: &str = "x-aegis-profile";
pub const RUN_HEADER: &str = "x-aegis-run";
/// Server capability: account and profile registration with per-request attribution.
pub const CAPABILITY_ACCOUNTS: &str = "accounts";
pub const ACCOUNT_HEADER: &str = "x-aegis-account";
/// Account vendors. Each maps to one route kind; `google` is reserved for agy.
pub const VENDORS: &[(&str, &str)] = &[
    ("anthropic", "claude_subscription"),
    ("openai", "codex_subscription"),
    ("google", "antigravity_subscription"),
];

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Provider {
    pub id: String,
    pub kind: String,
}

#[derive(Deserialize, Serialize)]
pub struct Connection {
    pub version: u32,
    pub providers: Vec<Provider>,
    /// Absent on older servers; clients must treat that as no optional capabilities.
    #[serde(default)]
    pub capabilities: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct Enrollment {
    pub providers: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct Release {
    pub version: String,
    pub protocol: u32,
    pub targets: Vec<String>,
}

/// Body of `PUT /api/relay/accounts/{id}`: one vendor sign-in bound to one route.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AccountRegistration {
    pub vendor: String,
    pub label: String,
    pub provider: String,
}

/// Body of `PUT /api/relay/profiles/{id}` under the `accounts` capability.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProfileLabel {
    pub label: String,
}
