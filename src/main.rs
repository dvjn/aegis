mod access_log;
mod api_keys;
mod app;
mod config;
mod db;
mod domain;
mod gateway;
mod health;
mod mcp;
mod migration;
mod oauth;
mod origin;
mod pricing;
mod providers;
mod request_id;
mod telemetry;
mod usage;
mod web;

use anyhow::{Context, Result, bail};
use api_keys::KeyStore;
use clap::{Parser, Subcommand};
use config::Config;
use domain::{Domain, DomainOptions, LogMailer, Mailer, SmtpMailer};
use sea_orm::DatabaseConnection;
use std::{collections::HashSet, path::PathBuf, sync::Arc};
use tokio::{net::TcpListener, signal};
use tokio_util::sync::CancellationToken;
use tracing::info;

#[derive(Parser)]
#[command(name = "aegis", version, about = "Personal LLM gateway")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Serve,
    Key {
        #[command(subcommand)]
        command: KeyCommand,
    },
    BootstrapUser {
        #[arg(long)]
        email: String,
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum KeyCommand {
    Create {
        #[arg(long)]
        user: uuid::Uuid,
        #[arg(long)]
        name: String,
        #[arg(long = "provider", required = true)]
        providers: Vec<String>,
    },
    List {
        #[arg(long)]
        user: uuid::Uuid,
    },
    Revoke {
        #[arg(long)]
        user: uuid::Uuid,
        id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .compact()
        .with_env_filter(access_log::filter())
        .init();

    let cli = Cli::parse();
    let config = Config::from_env()?;
    let database = db::connect(&config.database_url).await?;
    let result = match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(config, database.clone()).await,
        Command::Key { command } => manage_key(command, &config, database.clone()).await,
        Command::BootstrapUser {
            email,
            password_file,
        } => bootstrap_user(&config, &database, &email, password_file).await,
    };

    if let Err(error) = database.close().await {
        tracing::warn!(%error, "failed to close the database connection");
    }
    result
}

async fn serve(config: Config, database: DatabaseConnection) -> Result<()> {
    let mailer: Arc<dyn Mailer> = match config.smtp.clone() {
        Some(settings) => {
            Arc::new(SmtpMailer::new(settings).context("failed to initialize SMTP mailer")?)
        }
        None => Arc::new(LogMailer),
    };
    let domain = Arc::new(
        Domain::with_options(
            database.clone(),
            config.auth.clone(),
            config.oauth.clone(),
            DomainOptions {
                registration_enabled: config.registration_enabled,
                mailer,
            },
        )
        .await
        .context("failed to initialize identity services")?,
    );
    let keys = KeyStore::new(database.clone());
    let usage = usage::UsageStore::new(database.clone());
    let sink = telemetry::SqliteSink::new(database.clone());
    match sink.reconcile_interrupted().await {
        Ok(0) => {}
        Ok(closed) => tracing::info!(closed, "closed out interrupted gateway requests"),
        Err(error) => tracing::warn!(%error, "failed to close out interrupted gateway requests"),
    }
    let gateway = gateway::Gateway::new(
        sink,
        keys.clone(),
        config.providers,
        config.max_capture_bytes,
    )
    .context("failed to construct gateway")?;
    let cancellation = CancellationToken::new();
    match pricing::load_effective_map(&database, &config.pricing).await {
        Ok(map) => pricing::install(map),
        Err(error) => tracing::warn!(%error, "failed to load stored model prices"),
    }
    pricing::spawn_refresh(database.clone(), config.pricing, cancellation.clone());

    let application = app::router(
        domain,
        gateway,
        keys,
        usage,
        origin::OriginPolicy::new(config.public_url),
        cancellation.clone(),
    );
    let listener = TcpListener::bind(config.http_addr)
        .await
        .context("failed to bind HTTP listener")?;

    info!(address = %config.http_addr, "server listening");
    axum::serve(listener, application)
        .with_graceful_shutdown(shutdown_signal(cancellation))
        .await
        .context("HTTP server failed")?;
    info!("server stopped");
    Ok(())
}

async fn bootstrap_user(
    config: &Config,
    database: &DatabaseConnection,
    email: &str,
    password_file: Option<PathBuf>,
) -> Result<()> {
    let password = match password_file {
        Some(path) => std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read password from {}", path.display()))?
            .trim_end_matches(['\r', '\n'])
            .to_owned(),
        None => {
            let password = rpassword::prompt_password("Password: ")?;
            let confirmation = rpassword::prompt_password("Confirm password: ")?;
            if password != confirmation {
                bail!("passwords do not match");
            }
            password
        }
    };
    let password = domain::Password::new(password).map_err(anyhow::Error::msg)?;
    let id = domain::bootstrap_superuser(
        database,
        &config.auth.password_pepper,
        &config.auth.pepper_key_id,
        email,
        &password,
    )
    .await?;
    println!("created user: {id}");
    Ok(())
}

async fn manage_key(
    command: KeyCommand,
    config: &Config,
    database: DatabaseConnection,
) -> Result<()> {
    let keys = KeyStore::new(database);
    match command {
        KeyCommand::Create {
            user,
            name,
            providers,
        } => {
            let configured: HashSet<_> = config
                .providers
                .iter()
                .map(|provider| provider.id.as_str())
                .collect();
            let unknown: Vec<_> = providers
                .iter()
                .filter(|provider| !configured.contains(provider.as_str()))
                .collect();
            if !unknown.is_empty() {
                bail!("unknown provider IDs: {unknown:?}");
            }
            let (id, plaintext) = keys.create(user, &name, &providers).await?;
            println!("id: {id}");
            println!("key: {plaintext}");
            println!("Store this key now. Aegis will not display it again.");
        }
        KeyCommand::List { user } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&keys.list_for_user(user).await?)?
            );
        }
        KeyCommand::Revoke { user, id } => {
            if !keys.revoke(user, &id).await? {
                bail!("active key {id:?} was not found");
            }
            println!("revoked: {id}");
        }
    }
    Ok(())
}

async fn shutdown_signal(cancellation: CancellationToken) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let signal = tokio::select! {
        () = ctrl_c => "SIGINT",
        () = terminate => "SIGTERM",
    };
    info!(%signal, "shutdown signal received, draining requests");
    cancellation.cancel();
}
