//! Pairs this machine with one Aegis server, keeps its tokens fresh, and launches CLIs through it.

mod clis;
mod defaults;
mod launch;
mod proxy;
mod server;
pub mod store;

use std::{fmt, time::Duration};

use aegis_wire::{Machine, TokenResponse};
use anyhow::{Result, bail};
pub use clis::Cli;
pub use launch::{
    CliDefault, Launched, Offer, Pick, cli_defaults, launch, pick, remember, use_provider,
};
use server::{Poll, Server};
pub use store::{Login, Store};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub server: String,
    pub account: String,
    pub providers: Vec<String>,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Server:    {}", self.server)?;
        writeln!(f, "Account:   {}", self.account)?;
        write!(f, "Providers: {}", provider_text(&self.providers))
    }
}

pub fn provider_text(providers: &[String]) -> String {
    if providers.is_empty() {
        "none".to_owned()
    } else {
        providers.join(", ")
    }
}

/// The page and code the user approves in a browser.
pub struct Approval<'a> {
    pub url: &'a str,
    pub code: &'a str,
}

pub enum Logout {
    Revoked {
        server: String,
    },
    /// Forgotten here, but the server could not be told.
    Forgotten {
        server: String,
        reason: String,
    },
}

/// The server refused this machine's tokens, so its login was forgotten.
#[derive(Debug)]
pub struct LoggedOut {
    pub server: String,
    /// Why the stored login could not be forgotten, if it could not.
    pub kept: Option<String>,
}

impl fmt::Display for LoggedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let server = &self.server;
        write!(
            f,
            "{server} logged this machine out; run `ags login {server}` to pair it again"
        )?;
        match &self.kept {
            Some(error) => write!(f, " (could not forget this login: {error})"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for LoggedOut {}

fn logged_out(server: &str, forgotten: Result<()>) -> anyhow::Error {
    LoggedOut {
        server: server.to_owned(),
        kept: forgotten.err().map(|error| format!("{error:#}")),
    }
    .into()
}

/// Forgets `login` unless another ags process replaced it meanwhile.
async fn forget(store: &Store, login: &Login) -> anyhow::Error {
    let forgotten = async {
        let _lock = store.lock_async().await?;
        if store.load()?.as_ref() == Some(login) {
            store.remove()?;
        }
        Ok(())
    }
    .await;
    logged_out(&login.server, forgotten)
}

fn machine_name() -> String {
    hostname().map_or_else(|| "ags".to_owned(), |host| format!("ags on {host}"))
}

#[cfg(unix)]
fn hostname() -> Option<String> {
    let mut buffer = [0_u8; 256];
    // SAFETY: the buffer outlives the call and its length is passed with it.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let end = buffer.iter().position(|byte| *byte == 0)?;
    let name = String::from_utf8_lossy(&buffer[..end]).trim().to_owned();
    (!name.is_empty()).then_some(name)
}

#[cfg(not(unix))]
fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|name| !name.is_empty())
}

fn login_from(server: &str, client_id: &str, token: TokenResponse) -> Login {
    Login {
        server: server.to_owned(),
        client_id: client_id.to_owned(),
        expires_at: store::now() + token.expires_in,
        access_token: token.access_token,
        refresh_token: token.refresh_token,
    }
}

/// Pairs this machine with the server at `url`. `approve` shows the user where to approve it.
pub async fn login(
    store: &Store,
    url: &str,
    approve: impl FnOnce(&Approval<'_>),
) -> Result<Status> {
    let _lock = store.lock()?;
    if let Some(existing) = store.load()? {
        bail!(
            "already logged in to {}; run `ags logout` first",
            existing.server
        );
    }
    let server = Server::discover(url).await?;
    let client_id = server.register(&machine_name()).await?;
    let device = server.start_device(&client_id).await?;
    approve(&Approval {
        url: &device.verification_uri_complete,
        code: &device.user_code,
    });
    let deadline = store::now() + device.expires_in;
    let mut interval = device.interval.max(1);
    let token = loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        match server.poll(&client_id, &device.device_code).await? {
            Poll::Approved(token) => break token,
            Poll::Pending => {}
            Poll::SlowDown(next) => interval = next.max(interval),
            Poll::Denied => bail!("the sign-in was denied"),
            Poll::Expired => bail!("the code expired before it was approved"),
        }
        if store::now() >= deadline {
            bail!("the code expired before it was approved");
        }
    };
    let login = login_from(server.issuer(), &client_id, token);
    store.save(&login)?;
    match server.machine(&login.access_token).await? {
        Some(machine) => Ok(status_from(&login, machine)),
        None => Err(logged_out(&login.server, store.remove())),
    }
}

/// The stored login, if any, with its access token refreshed shortly before it expires.
pub async fn current_login(store: &Store) -> Result<Option<Login>> {
    let Some(login) = store.load()? else {
        return Ok(None);
    };
    if !login.expires_soon() {
        return Ok(Some(login));
    }
    let _lock = store.lock_async().await?;
    // Another ags process may have refreshed while this one waited for the lock.
    match store.load()? {
        Some(login) => fresh(store, login).await.map(Some),
        None => Ok(None),
    }
}

/// Refreshes `login` when it expires soon. The caller holds the store lock.
async fn fresh(store: &Store, login: Login) -> Result<Login> {
    if !login.expires_soon() {
        return Ok(login);
    }
    let server = Server::discover(&login.server).await?;
    let Some(refresh_token) = &login.refresh_token else {
        return Err(logged_out(&login.server, store.remove()));
    };
    let Some(token) = server.refresh(&login.client_id, refresh_token).await? else {
        return Err(logged_out(&login.server, store.remove()));
    };
    let mut refreshed = login_from(&login.server, &login.client_id, token);
    if refreshed.refresh_token.is_none() {
        refreshed.refresh_token = login.refresh_token.clone();
    }
    store.save(&refreshed)?;
    Ok(refreshed)
}

fn status_from(login: &Login, machine: Machine) -> Status {
    Status {
        server: login.server.clone(),
        account: machine.account,
        providers: machine.providers,
    }
}

/// `None` when this machine is not logged in.
pub async fn status(store: &Store) -> Result<Option<Status>> {
    let Some(login) = current_login(store).await? else {
        return Ok(None);
    };
    let server = Server::discover(&login.server).await?;
    match server.machine(&login.access_token).await? {
        Some(machine) => Ok(Some(status_from(&login, machine))),
        None => Err(forget(store, &login).await),
    }
}

/// Forgets the server even when it cannot be told to revoke this machine.
pub async fn logout(store: &Store) -> Result<Option<Logout>> {
    let _lock = store.lock()?;
    let Some(stored) = store.load()? else {
        return Ok(None);
    };
    let server = stored.server.clone();
    let revoked = async {
        let login = match fresh(store, stored).await {
            Ok(login) => login,
            // A rejected refresh token means the grant is already gone.
            Err(error) if error.is::<LoggedOut>() => return Ok(()),
            Err(error) => return Err(error),
        };
        Server::discover(&login.server)
            .await?
            .unpair(&login.access_token)
            .await
            .map(|_| ())
    }
    .await;
    store.remove()?;
    Ok(Some(match revoked {
        Ok(()) => Logout::Revoked { server },
        Err(error) => Logout::Forgotten {
            server,
            reason: format!("{error:#}"),
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_lists_the_server_account_and_providers() {
        let status = Status {
            server: "https://aegis.example".into(),
            account: "me@example.com".into(),
            providers: vec!["claude".into(), "codex".into()],
        };
        assert_eq!(
            status.to_string(),
            "Server:    https://aegis.example\nAccount:   me@example.com\nProviders: claude, codex"
        );
        assert_eq!(provider_text(&[]), "none");
    }

    #[test]
    fn a_machine_is_named_after_its_host() {
        assert!(machine_name().starts_with("ags"));
    }

    #[tokio::test]
    async fn logging_out_without_a_login_does_nothing() {
        let store = Store::at(store::tests::scratch().join("ags"));
        assert!(logout(&store).await.unwrap().is_none());
        assert!(status(&store).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_second_login_is_refused_until_logout() {
        let dir = store::tests::scratch();
        let store = Store::at(&dir);
        store
            .save(&Login {
                server: "https://aegis.example".into(),
                client_id: "client".into(),
                access_token: "token".into(),
                refresh_token: None,
                expires_at: store::now() + 3600,
            })
            .unwrap();
        let error = login(&store, "https://other.example", |_| {})
            .await
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "already logged in to https://aegis.example; run `ags logout` first"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    use axum::http::StatusCode;

    const INVALID_GRANT: (StatusCode, &str) =
        (StatusCode::BAD_REQUEST, r#"{"error":"invalid_grant"}"#);
    const UNAUTHORIZED: (StatusCode, &str) = (StatusCode::UNAUTHORIZED, "");

    /// A server whose token and machine endpoints always answer `token` and `machine`.
    async fn fake_server(
        token: (StatusCode, &'static str),
        machine: (StatusCode, &'static str),
    ) -> String {
        use aegis_wire::{AuthorizationServerMetadata, MACHINE_PATH, METADATA_PATH};
        use axum::{
            Json, Router,
            http::header,
            routing::{any, get, post},
        };

        let reply = |(status, body): (StatusCode, &'static str)| {
            move || async move { (status, [(header::CONTENT_TYPE, "application/json")], body) }
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let metadata = AuthorizationServerMetadata {
            issuer: address.clone(),
            authorization_endpoint: format!("{address}/oauth/authorize"),
            token_endpoint: format!("{address}/oauth/token"),
            device_authorization_endpoint: format!("{address}/oauth/device"),
            registration_endpoint: format!("{address}/oauth/register"),
            response_types_supported: vec![],
            grant_types_supported: vec![],
            token_endpoint_auth_methods_supported: vec![],
            code_challenge_methods_supported: vec![],
            scopes_supported: vec![],
            authorization_response_iss_parameter_supported: true,
            resource_parameter_supported: true,
        };
        let app = Router::new()
            .route(METADATA_PATH, get(move || async move { Json(metadata) }))
            .route("/oauth/token", post(reply(token)))
            .route(MACHINE_PATH, any(reply(machine)));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        address
    }

    fn stored(server: &str, refresh_token: Option<&str>, expires_at: u64) -> Login {
        Login {
            server: server.to_owned(),
            client_id: "client".into(),
            access_token: "ag_at1.a.b".into(),
            refresh_token: refresh_token.map(str::to_owned),
            expires_at,
        }
    }

    #[tokio::test]
    async fn a_refused_login_is_forgotten_with_one_explanation() {
        let expiring = store::now();
        let valid = store::now() + 3600;
        for (case, token, refresh_token, expires_at) in [
            (
                "refused refresh",
                INVALID_GRANT,
                Some("ag_rt1.a.b"),
                expiring,
            ),
            ("no refresh token", INVALID_GRANT, None, expiring),
            (
                "refused access token",
                INVALID_GRANT,
                Some("ag_rt1.a.b"),
                valid,
            ),
        ] {
            let server = fake_server(token, UNAUTHORIZED).await;
            let dir = store::tests::scratch();
            let store = Store::at(&dir);
            let login = stored(&server, refresh_token, expires_at);

            store.save(&login).unwrap();
            let error = status(&store).await.err().unwrap();
            assert!(error.is::<LoggedOut>(), "{case}");
            assert_eq!(
                error.to_string(),
                format!(
                    "{server} logged this machine out; run `ags login {server}` to pair it again"
                ),
                "{case}"
            );
            assert!(
                store.load().unwrap().is_none(),
                "{case}: `ags login` works next"
            );

            store.save(&login).unwrap();
            assert!(
                matches!(
                    logout(&store).await.unwrap(),
                    Some(Logout::Revoked { server: logged_out }) if logged_out == server
                ),
                "{case}"
            );
            assert!(store.load().unwrap().is_none(), "{case}");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[tokio::test]
    async fn a_refresh_that_fails_otherwise_keeps_the_login() {
        for (case, token) in [
            ("server error", (StatusCode::INTERNAL_SERVER_ERROR, "")),
            ("unreadable success", (StatusCode::OK, "<html>")),
            ("unreadable refusal", (StatusCode::BAD_REQUEST, "<html>")),
            (
                "other OAuth error",
                (StatusCode::BAD_REQUEST, r#"{"error":"invalid_request"}"#),
            ),
        ] {
            let server = fake_server(token, UNAUTHORIZED).await;
            let dir = store::tests::scratch();
            let store = Store::at(&dir);
            let login = stored(&server, Some("ag_rt1.a.b"), store::now());

            store.save(&login).unwrap();
            let error = status(&store).await.err().unwrap();
            assert!(!error.is::<LoggedOut>(), "{case}: {error:#}");
            assert!(store.load().unwrap() == Some(login), "{case}");
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn a_login_that_could_not_be_forgotten_still_says_why_it_ended() {
        let error = logged_out(
            "https://aegis.example",
            Err(anyhow::anyhow!("removing login.json: permission denied")),
        );
        assert_eq!(
            error.to_string(),
            "https://aegis.example logged this machine out; run `ags login https://aegis.example` to pair it again (could not forget this login: removing login.json: permission denied)"
        );
    }

    #[tokio::test]
    async fn a_server_url_needs_a_scheme() {
        let store = Store::at(store::tests::scratch().join("ags"));
        let error = login(&store, "aegis.example", |_| {}).await.err().unwrap();
        assert!(error.to_string().starts_with("give the server's full URL"));
    }
}
