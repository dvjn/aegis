mod access_log;
mod analytics;
mod analytics_facts;
mod api_keys;
mod app;
mod compression;
mod config;
mod db;
mod domain;
mod gateway;
mod health;
mod jobs;
mod mcp;
mod migration;
mod oauth;
mod origin;
mod payload_facts;
mod pricing;
mod providers;
mod request_id;
mod request_metrics;
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
    let reporting_database = db::reporting_connection(&config.database_url, &database).await?;
    let usage = usage::UsageStore::new(reporting_database);
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
        Ok(map) => {
            pricing::install(map.clone());
            if let Err(error) = pricing::backfill_costs(&database, &map).await {
                tracing::warn!(%error, "failed to backfill historical request costs");
            }
        }
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

    let analytics = match start_analytics(&config.analytics, &database, cancellation.clone()).await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "analytics unavailable; capture remains enabled");
            None
        }
    };
    info!(address = %config.http_addr, "server listening");
    jobs::spawn(database.clone());
    let result = axum::serve(listener, application)
        .with_graceful_shutdown(shutdown_signal(cancellation.clone()))
        .await
        .context("HTTP server failed");
    cancellation.cancel();
    if let Some(runtime) = analytics {
        runtime.shutdown().await;
    }
    result?;
    info!("server stopped");
    Ok(())
}

struct AnalyticsRuntime {
    worker: tokio::task::JoinHandle<()>,
    status: tokio::task::JoinHandle<()>,
    reader: DatabaseConnection,
}

impl AnalyticsRuntime {
    async fn shutdown(self) {
        if let Err(error) = self.worker.await {
            tracing::warn!(%error, "analytics worker stopped unexpectedly");
        }
        if let Err(error) = self.status.await {
            tracing::warn!(%error, "analytics status observer stopped unexpectedly");
        }
        if let Err(error) = self.reader.close().await {
            tracing::warn!(%error, "failed to close analytics reader");
        }
    }
}

async fn start_analytics(
    config: &config::AnalyticsConfig,
    database: &DatabaseConnection,
    cancellation: CancellationToken,
) -> Result<Option<AnalyticsRuntime>> {
    if !config.enabled {
        return Ok(None);
    }
    let source_path = database
        .get_sqlite_connection_pool()
        .connect_options()
        .get_filename()
        .to_path_buf();
    if let Some(parent) = config
        .database_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).context("failed to create analytics directory")?;
    }
    let source = analytics::source::Source::open(database.clone(), &source_path).await?;
    let store = analytics::sqlite::SqliteStore::open(
        &source_path,
        &config.database_path,
        &source.source_id,
    )
    .await?;
    // Keep a separate read-only pool ready; dashboard cutover is a later, gated layer.
    let reader = match store.reader().await {
        Ok(reader) => reader,
        Err(error) => {
            store.close().await?;
            return Err(error);
        }
    };
    let worker = match analytics::worker::Worker::new(
        source,
        store,
        analytics::source::Limits {
            request_count: config.batch_requests,
            child_rows: config.max_child_rows,
            bytes: config.max_batch_bytes,
            snapshot_duration: std::time::Duration::from_millis(config.max_snapshot_ms),
        },
        std::time::Duration::from_secs(config.interval_seconds),
        analytics::worker::MAX_BATCHES_PER_ATTEMPT,
    )
    .await
    {
        Ok(worker) => worker,
        Err(error) => {
            reader.close().await?;
            return Err(error);
        }
    };
    let mut status = worker.status();
    let status = tokio::spawn(async move {
        while status.changed().await.is_ok() {
            let current = status.borrow_and_update().clone();
            if let analytics::worker::Availability::Unavailable(error) = &current.availability {
                tracing::warn!(%error, "analytics projection unavailable");
            }
            tracing::info!(state = ?current.availability,
                published_at = ?current.published.as_ref().map(|b| b.observed_at.as_str()),
                published_revision = ?current.published.as_ref().map(|b| b.revision),
                pending_count = ?current.pending_count,
                quarantined_count = current.quarantined.len(),
                oldest_pending_at = ?current.oldest_pending_at,
                processing_ms = current.processing_duration.as_millis() as u64,
                batch_ms = current.last_batch_duration.as_millis() as u64,
                "analytics projection status");
        }
    });
    Ok(Some(AnalyticsRuntime {
        worker: tokio::spawn(worker.run(cancellation)),
        status,
        reader,
    }))
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
