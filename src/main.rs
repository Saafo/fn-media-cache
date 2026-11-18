mod automation_store;
mod config;
mod doctor;
mod engine;
mod fntv;
mod library;
mod local_history;
mod logging;
mod media_trace;
mod partial_cache;
mod power;
mod prefetch;
mod privacy;
mod probe;
mod protocol;
mod proxy_requests;
mod recovery;
mod routes;
mod scheduler;
mod segments;
mod server;
mod session_recovery;
mod store;
mod transfers;

use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use fs2::FileExt;

use config::{Config, private_dir, private_file};
use engine::Engine;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a configuration without touching an existing file.
    Init {
        #[arg(long)]
        upstream: String,
        #[arg(long, default_value = "fn-media-cache.toml")]
        config: PathBuf,
    },
    /// Run the proxy, local management page and resumable download worker.
    Serve {
        /// Defaults to fn-media-cache.toml, with support for an existing fn-media.toml.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Desktop child protocol: announce readiness on stdout and stop when stdin closes.
        #[arg(long, hide = true)]
        parent_pipe: bool,
    },
    /// Check the configured local management and proxy endpoints without credentials.
    Doctor {
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(1..=120))]
        timeout_seconds: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Init { upstream, config } => {
            let settings = Config {
                upstream,
                ..Config::default()
            };
            settings.validate()?;
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&config)
                .context("cannot create config; an existing file will not be overwritten")?;
            file.write_all(toml::to_string_pretty(&settings)?.as_bytes())?;
            private_file(&config)?;
            println!("Created {}", config.display());
        }
        Command::Serve {
            config,
            parent_pipe,
        } => {
            let config = config.unwrap_or_else(|| config::configuration_path(Path::new(".")));
            if let Err(error) = serve(Config::load(&config)?, parent_pipe).await {
                event!("service stopped with error: {error}");
                return Err(error);
            }
        }
        Command::Doctor {
            config,
            timeout_seconds,
        } => {
            let config = config.unwrap_or_else(|| config::configuration_path(Path::new(".")));
            doctor::run(
                &Config::load(&config)?,
                Duration::from_secs(timeout_seconds),
            )
            .await?;
        }
    }
    Ok(())
}

async fn serve(config: Config, parent_pipe: bool) -> Result<()> {
    private_dir(&config.data_dir)?;
    private_dir(&config.data_dir.join("cache"))?;
    private_dir(&config.data_dir.join("logs"))?;
    let lock_path = config.data_dir.join("instance.lock");
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)?;
    private_file(&lock_path)?;
    lock.try_lock_exclusive()
        .context("another instance is already using this data directory")?;
    logging::initialize(
        &config.data_dir.join("logs"),
        config.log_max_bytes,
        config.log_keep_files,
    )?;
    event!("service starting version={}", env!("CARGO_PKG_VERSION"));
    let token_path = config.data_dir.join("admin-token");
    let token = match fs::read_to_string(&token_path) {
        Ok(token) => token.trim().to_owned(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let token = format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            );
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&token_path)?;
            private_file(&token_path)?;
            file.write_all(token.as_bytes())?;
            token
        }
        Err(error) => return Err(error.into()),
    };
    if token.len() < 32 {
        bail!("management token file is invalid");
    }
    let store = store::Store::open(&config.data_dir, config.upstream.trim_end_matches('/'))?;
    let proxy_listener = tokio::net::TcpListener::bind(config.proxy_listen)
        .await
        .context("cannot bind proxy listener")?;
    let admin_listener = tokio::net::TcpListener::bind(config.admin_listen)
        .await
        .context("cannot bind management listener")?;
    let engine = Engine::new(config, store, token)?;
    if parent_pipe {
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "ready")?;
        stdout.flush()?;
    } else if std::env::var_os("FN_MEDIA_CACHE_MANAGED").is_none()
        && std::env::var_os("FN_MEDIA_MANAGED").is_none()
    {
        println!("Proxy: http://{}", proxy_listener.local_addr()?);
        println!("Management: http://{}", admin_listener.local_addr()?);
        println!("Management token file: {}", token_path.display());
        println!("Press Ctrl+C to stop. Downloads resume on the next start.");
    }
    let cancel = engine.shutdown.clone();
    let mut proxy = tokio::spawn(
        axum::serve(
            proxy_listener,
            server::proxy_router(Arc::clone(&engine))
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(cancel.clone().cancelled_owned())
        .into_future(),
    );
    let mut admin = tokio::spawn(
        axum::serve(admin_listener, server::admin_router(Arc::clone(&engine)))
            .with_graceful_shutdown(cancel.clone().cancelled_owned())
            .into_future(),
    );
    let mut worker = tokio::spawn(engine.clone().worker());
    let mut scheduler = tokio::spawn(engine.clone().scheduler());
    let power = tokio::spawn(power::monitor(engine.clone()));
    let result = tokio::select! {
        result = shutdown_signal() => result.context("cannot listen for shutdown signal"),
        result = parent_closed(parent_pipe) => result.context("cannot watch desktop parent pipe"),
        result = &mut proxy => result.context("proxy task failed").and_then(|r| r.context("proxy stopped")),
        result = &mut admin => result.context("management task failed").and_then(|r| r.context("management stopped")),
        _ = &mut worker => Err(anyhow::anyhow!("download worker stopped unexpectedly")),
        _ = &mut scheduler => Err(anyhow::anyhow!("scheduler stopped unexpectedly")),
    };
    cancel.cancel();
    let _ = power.await;
    event!("service shutdown requested; ok={}", result.is_ok());
    // Avoid waiting forever for an abandoned client. Drop aborts only after cooperative shutdown.
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        while !worker.is_finished()
            || !scheduler.is_finished()
            || !proxy.is_finished()
            || !admin.is_finished()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    proxy.abort();
    admin.abort();
    worker.abort();
    scheduler.abort();
    drop(lock);
    result
}

async fn parent_closed(enabled: bool) -> std::io::Result<()> {
    if !enabled {
        return std::future::pending().await;
    }
    let (closed, receiver) = tokio::sync::oneshot::channel();
    // A detached reader will not hold the runtime open if a signal stops the service first.
    std::thread::Builder::new()
        .name("desktop-parent-pipe".into())
        .spawn(move || {
            let result = std::io::copy(&mut std::io::stdin().lock(), &mut std::io::sink());
            let _ = closed.send(result.map(|_| ()));
        })?;
    receiver.await.map_err(std::io::Error::other)?
}

async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}
