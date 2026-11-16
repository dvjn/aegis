//! The contract shared by the Aegis server and the `ags` CLI.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Carries an API key, or a paired machine's access token, to the gateway.
pub const CREDENTIAL_HEADER: &str = "x-aegis-api-key";
/// Lets an OAuth access token stand in for an API key at the gateway.
pub const GATEWAY_SCOPE: &str = "gateway:use";
pub const OFFLINE_ACCESS_SCOPE: &str = "offline_access";
/// A paired machine's tokens are issued for this resource, appended to the issuer.
pub const GATEWAY_RESOURCE_PATH: &str = "/providers";
pub const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
pub const REFRESH_TOKEN_GRANT_TYPE: &str = "refresh_token";

pub const METADATA_PATH: &str = "/.well-known/oauth-authorization-server";
/// The paired machine's own view of its grant; `DELETE` unpairs it.
pub const MACHINE_PATH: &str = "/api/machine";

/// The type names of providers, as the server stores and reports them.
pub mod provider_type {
    pub const CLAUDE_SUBSCRIPTION: &str = "claude_subscription";
    pub const CODEX_SUBSCRIPTION: &str = "codex_subscription";
    pub const TYPESAFE: &str = "typesafe";
    pub const BEDROCK: &str = "bedrock";
    pub const VERTEX: &str = "vertex";
}

/// Names the `ags` launch that a paired machine's gateway request belongs to.
pub const LAUNCH_ID_HEADER: &str = "x-aegis-launch-id";
pub const MAX_LAUNCH_ID_LEN: usize = 128;

/// A launch ID is 1 to 128 printable ASCII characters; `ags` sends a random UUID.
pub fn is_launch_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_LAUNCH_ID_LEN
        && value.bytes().all(|byte| (b' '..=b'~').contains(&byte))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthorizationServerMetadata {
    pub issuer: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub device_authorization_endpoint: String,
    pub registration_endpoint: String,
    pub response_types_supported: Vec<String>,
    pub grant_types_supported: Vec<String>,
    pub token_endpoint_auth_methods_supported: Vec<String>,
    pub code_challenge_methods_supported: Vec<String>,
    pub scopes_supported: Vec<String>,
    pub authorization_response_iss_parameter_supported: bool,
    pub resource_parameter_supported: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClientRegistrationRequest {
    #[serde(default)]
    pub redirect_uris: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_endpoint_auth_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_type: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientRegistrationResponse {
    pub client_id: String,
    pub client_id_issued_at: i64,
    pub redirect_uris: Vec<String>,
    pub client_name: Option<String>,
    pub token_endpoint_auth_method: String,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    pub scope: String,
    pub application_type: String,
}

/// No `Debug`, so the device code never reaches a log.
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceAuthorizationResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// No `Debug`, so the tokens never reach a log.
#[derive(Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OAuthError {
    pub error: String,
    /// The new polling interval, sent with `slow_down`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    pub account: String,
    pub name: String,
    pub providers: Vec<String>,
    /// The type of each granted provider that still exists, by ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_types: BTreeMap<String, String>,
    /// The AWS region of each granted Bedrock provider whose endpoint names one, by ID. `None` from
    /// a server too old to report regions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_regions: Option<BTreeMap<String, String>>,
    /// The location of each granted Vertex provider whose endpoint serves one location only, by ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_locations: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_omits_an_interval_it_does_not_have() {
        let pending = OAuthError {
            error: "authorization_pending".into(),
            interval: None,
        };
        assert_eq!(
            serde_json::to_string(&pending).unwrap(),
            r#"{"error":"authorization_pending"}"#
        );
        let slow: OAuthError =
            serde_json::from_str(r#"{"error":"slow_down","interval":10}"#).unwrap();
        assert_eq!(slow.interval, Some(10));
    }

    #[test]
    fn a_launch_id_is_short_printable_ascii() {
        assert!(is_launch_id("0b6f0c3e-8d1a-4c47-9e0f-2f7d3c1a5b9e"));
        assert!(is_launch_id(&"a".repeat(MAX_LAUNCH_ID_LEN)));
        let too_long = "a".repeat(MAX_LAUNCH_ID_LEN + 1);
        for bad in ["", "tab\there", "caf\u{e9}", too_long.as_str()] {
            assert!(!is_launch_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_machine_from_a_server_without_provider_types_still_reads() {
        let machine: Machine = serde_json::from_str(
            r#"{"account":"me@example.com","name":"ags","providers":["claude"]}"#,
        )
        .unwrap();
        assert!(machine.provider_types.is_empty());
        assert!(machine.provider_regions.is_none());
        assert!(machine.provider_locations.is_empty());
    }

    #[test]
    fn a_registration_sends_only_what_it_sets() {
        let request = ClientRegistrationRequest {
            client_name: Some("ags".into()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"redirect_uris":[],"client_name":"ags"}"#
        );
    }
}
