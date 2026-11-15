//! The OAuth and machine endpoints of one Aegis server.
use std::{sync::Once, time::Duration};

use aegis_wire::{
    AuthorizationServerMetadata, ClientRegistrationRequest, ClientRegistrationResponse,
    DEVICE_GRANT_TYPE, DeviceAuthorizationResponse, GATEWAY_RESOURCE_PATH, GATEWAY_SCOPE,
    MACHINE_PATH, METADATA_PATH, Machine, OAuthError, OFFLINE_ACCESS_SCOPE,
    REFRESH_TOKEN_GRANT_TYPE, TokenResponse,
};
use anyhow::{Context, Result, anyhow, bail};
use reqwest::{Response, StatusCode};
use url::{Host, Origin, Url};

const TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Server {
    http: reqwest::Client,
    origin: Origin,
    /// The origin as text; it is also the issuer.
    address: String,
    metadata: AuthorizationServerMetadata,
}

pub(crate) enum Poll {
    Pending,
    SlowDown(u64),
    Approved(TokenResponse),
    Denied,
    Expired,
}

fn http() -> Result<reqwest::Client> {
    static PROVIDER: Once = Once::new();
    PROVIDER.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    reqwest::Client::builder()
        .user_agent(format!("ags/{}", aegis_wire::VERSION))
        .timeout(TIMEOUT)
        // Every endpoint is checked to be on the server's origin, so a redirect could only leave it.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("starting the HTTP client")
}

async fn oauth_error(response: Response) -> OAuthError {
    let status = response.status();
    response.json().await.unwrap_or(OAuthError {
        error: format!("HTTP {status}"),
        interval: None,
    })
}

fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    }
}

fn on_origin(url: &str, origin: &Origin) -> bool {
    Url::parse(url).is_ok_and(|url| url.origin() == *origin)
}

impl Server {
    /// Reads the metadata of the server at the origin of `url`, which must also be its issuer.
    pub(crate) async fn discover(url: &str) -> Result<Self> {
        let parsed = Url::parse(url)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.has_host())
            .ok_or_else(|| anyhow!("give the server's full URL, such as https://aegis.example"))?;
        let origin = parsed.origin();
        let address = origin.ascii_serialization();
        if parsed.scheme() == "http" && !is_loopback(&parsed) {
            bail!("{address} uses plain http; use its https address instead");
        }
        let http = http()?;
        let response = http
            .get(format!("{address}{METADATA_PATH}"))
            .send()
            .await
            .with_context(|| format!("could not reach {address}"))?;
        if !response.status().is_success() {
            bail!("{address} is not an Aegis server ({})", response.status());
        }
        let metadata: AuthorizationServerMetadata = response
            .json()
            .await
            .with_context(|| format!("{address} is not an Aegis server"))?;
        let issuer_matches = Url::parse(&metadata.issuer).is_ok_and(|issuer| {
            issuer.origin() == origin
                && matches!(issuer.path(), "" | "/")
                && issuer.query().is_none()
        });
        if !issuer_matches {
            bail!(
                "{address} says its address is {}; run `ags login` with that address",
                metadata.issuer
            );
        }
        let endpoints = [
            &metadata.token_endpoint,
            &metadata.registration_endpoint,
            &metadata.device_authorization_endpoint,
        ];
        if !endpoints
            .iter()
            .all(|endpoint| on_origin(endpoint, &origin))
        {
            bail!("{address} sends sign-in requests to another address; refusing to log in");
        }
        Ok(Self {
            http,
            origin,
            address,
            metadata,
        })
    }

    pub(crate) fn issuer(&self) -> &str {
        &self.address
    }

    fn resource(&self) -> String {
        format!("{}{GATEWAY_RESOURCE_PATH}", self.issuer())
    }

    pub(crate) async fn register(&self, name: &str) -> Result<String> {
        let response = self
            .http
            .post(&self.metadata.registration_endpoint)
            .json(&ClientRegistrationRequest {
                client_name: Some(name.to_owned()),
                token_endpoint_auth_method: Some("none".into()),
                grant_types: Some(vec![
                    DEVICE_GRANT_TYPE.into(),
                    REFRESH_TOKEN_GRANT_TYPE.into(),
                ]),
                response_types: Some(Vec::new()),
                scope: Some(format!("{GATEWAY_SCOPE} {OFFLINE_ACCESS_SCOPE}")),
                application_type: Some("native".into()),
                ..Default::default()
            })
            .send()
            .await
            .context("registering this machine")?;
        if response.status() != StatusCode::CREATED {
            // Servers without machine pairing reject the gateway scope as unknown.
            bail!(
                "{} refused to register this machine; it may need a newer Aegis",
                self.issuer()
            );
        }
        let registered: ClientRegistrationResponse = response.json().await?;
        Ok(registered.client_id)
    }

    pub(crate) async fn start_device(
        &self,
        client_id: &str,
    ) -> Result<DeviceAuthorizationResponse> {
        let response = self
            .http
            .post(&self.metadata.device_authorization_endpoint)
            .form(&[("client_id", client_id), ("resource", &self.resource())])
            .send()
            .await
            .context("starting the sign-in")?;
        if !response.status().is_success() {
            bail!(
                "the server refused to start the sign-in: {}",
                oauth_error(response).await.error
            );
        }
        let device: DeviceAuthorizationResponse = response.json().await?;
        if !on_origin(&device.verification_uri_complete, &self.origin) {
            bail!("{} sent an approval page on another address", self.issuer());
        }
        Ok(device)
    }

    pub(crate) async fn poll(&self, client_id: &str, device_code: &str) -> Result<Poll> {
        let response = self
            .http
            .post(&self.metadata.token_endpoint)
            .form(&[
                ("grant_type", DEVICE_GRANT_TYPE),
                ("client_id", client_id),
                ("device_code", device_code),
                ("resource", &self.resource()),
            ])
            .send()
            .await
            .context("waiting for approval")?;
        if response.status().is_success() {
            return Ok(Poll::Approved(response.json().await?));
        }
        let error = oauth_error(response).await;
        Ok(match error.error.as_str() {
            "authorization_pending" => Poll::Pending,
            "slow_down" => Poll::SlowDown(error.interval.unwrap_or(10)),
            "access_denied" => Poll::Denied,
            "expired_token" => Poll::Expired,
            other => bail!("the sign-in failed: {other}"),
        })
    }

    /// `None` when the server no longer accepts the refresh token.
    pub(crate) async fn refresh(
        &self,
        client_id: &str,
        refresh_token: &str,
    ) -> Result<Option<TokenResponse>> {
        let response = self
            .http
            .post(&self.metadata.token_endpoint)
            .form(&[
                ("grant_type", REFRESH_TOKEN_GRANT_TYPE),
                ("client_id", client_id),
                ("refresh_token", refresh_token),
                ("resource", &self.resource()),
            ])
            .send()
            .await
            .context("refreshing this machine's token")?;
        if response.status().is_success() {
            return Ok(Some(response.json().await?));
        }
        let error = oauth_error(response).await;
        if error.error == "invalid_grant" {
            return Ok(None);
        }
        bail!("could not refresh this machine's token: {}", error.error)
    }

    fn machine_request(&self, method: reqwest::Method, token: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{MACHINE_PATH}", self.issuer()))
            .bearer_auth(token)
    }

    /// `None` when the server no longer accepts the access token.
    pub(crate) async fn machine(&self, token: &str) -> Result<Option<Machine>> {
        let response = self
            .machine_request(reqwest::Method::GET, token)
            .send()
            .await
            .context("reading this machine's access")?;
        match response.status() {
            StatusCode::UNAUTHORIZED => Ok(None),
            status if status.is_success() => Ok(Some(response.json().await?)),
            _ => bail!(
                "could not read this machine's access: {}",
                oauth_error(response).await.error
            ),
        }
    }

    /// Revokes the machine's grant. `false` when it was already revoked.
    pub(crate) async fn unpair(&self, token: &str) -> Result<bool> {
        let response = self
            .machine_request(reqwest::Method::DELETE, token)
            .send()
            .await
            .context("revoking this machine's access")?;
        match response.status() {
            StatusCode::UNAUTHORIZED => Ok(false),
            status if status.is_success() => Ok(true),
            _ => bail!(
                "could not revoke this machine's access: {}",
                oauth_error(response).await.error
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_only_for_this_machine() {
        for url in [
            "http://localhost:8765",
            "http://127.0.0.1:8765",
            "http://[::1]:8765",
        ] {
            assert!(is_loopback(&Url::parse(url).unwrap()), "{url}");
        }
        for url in [
            "http://aegis.example",
            "http://10.0.0.1",
            "http://localhost.example",
        ] {
            assert!(!is_loopback(&Url::parse(url).unwrap()), "{url}");
        }
    }

    #[tokio::test]
    async fn a_remote_server_over_plain_http_is_refused_before_any_request() {
        let error = Server::discover("http://aegis.example")
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "http://aegis.example uses plain http; use its https address instead"
        );
    }

    #[test]
    fn an_endpoint_counts_only_on_the_exact_origin() {
        let origin = Url::parse("https://aegis.example").unwrap().origin();
        assert!(on_origin("https://aegis.example/oauth/token", &origin));
        assert!(on_origin("https://aegis.example:443/oauth/token", &origin));
        for other in [
            "http://aegis.example/oauth/token",
            "https://aegis.example:8443/oauth/token",
            "https://evil.example/oauth/token",
            "javascript:alert(1)",
            "not a url",
        ] {
            assert!(!on_origin(other, &origin), "{other}");
        }
    }
}
