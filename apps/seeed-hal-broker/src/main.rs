#![forbid(unsafe_code)]

mod manifest;

use std::io;
use std::path::{Path, PathBuf};

use clap::{Parser, ValueEnum};
use seeed_hal_adapter_serialport::SerialPortAdapter;
use seeed_hal_broker::{Broker, StartupToken};
use seeed_hal_runtime::HalRuntime;
use serde::Serialize;
use tokio::io::AsyncReadExt;
use tokio::task::JoinSet;

const MAX_CONNECTIONS: usize = 64;

#[derive(Parser)]
#[command(version, about = "Local hardware access broker")]
struct Args {
    #[arg(long, required_unless_present = "manifest")]
    endpoint: Option<PathBuf>,
    #[arg(long, required_unless_present = "manifest")]
    auth_token_file: Option<PathBuf>,
    #[arg(long)]
    manifest: bool,
    #[arg(long, value_enum, default_value_t = LogFormat::Json)]
    log_format: LogFormat,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Json,
    Pretty,
}

#[derive(Serialize)]
struct Readiness<'a> {
    status: &'static str,
    endpoint: &'a str,
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("seeed-hal-broker: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.manifest {
        println!(
            "{}",
            serde_json::to_string(&manifest::BrokerManifest::current())?
        );
        return Ok(());
    }

    install_tracing(args.log_format)?;
    let endpoint = args.endpoint.expect("clap requires --endpoint");
    let token_path = args
        .auth_token_file
        .expect("clap requires --auth-token-file");
    let token = read_and_remove_token(&token_path).await?;
    let runtime = HalRuntime::builder()
        .serial_adapter(SerialPortAdapter::new())
        .build();
    let broker = Broker::with_startup_token(runtime, StartupToken::from_bytes(token));
    serve(endpoint, broker).await?;
    Ok(())
}

fn install_tracing(format: LogFormat) -> io::Result<()> {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let result = match format {
        LogFormat::Json => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .json()
            .try_init(),
        LogFormat::Pretty => tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .pretty()
            .try_init(),
    };
    result.map_err(io::Error::other)
}

async fn read_and_remove_token(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = tokio::fs::OpenOptions::new().read(true).open(path).await?;
    let mut token = [0_u8; 32];
    file.read_exact(&mut token).await?;
    let mut extra = [0_u8; 1];
    if file.read(&mut extra).await? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "authentication token file must contain exactly 32 bytes",
        ));
    }
    drop(file);
    tokio::fs::remove_file(path).await?;
    Ok(token)
}

fn print_readiness(endpoint: &Path) -> Result<(), serde_json::Error> {
    let endpoint = endpoint.to_string_lossy();
    println!(
        "{}",
        serde_json::to_string(&Readiness {
            status: "ready",
            endpoint: &endpoint,
        })?
    );
    Ok(())
}

#[cfg(unix)]
async fn serve(endpoint: PathBuf, broker: Broker) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::PermissionsExt;

    let parent = endpoint.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "endpoint must have a parent directory",
        )
    })?;
    let parent_mode = tokio::fs::metadata(parent).await?.permissions().mode();
    if parent_mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "endpoint parent directory must not be accessible by group or other users",
        )
        .into());
    }
    let listener = tokio::net::UnixListener::bind(&endpoint)?;
    tokio::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).await?;
    let cleanup = UnixSocketCleanup(endpoint.clone());
    print_readiness(&endpoint)?;

    let mut connections = JoinSet::new();
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                match permits.clone().try_acquire_owned() {
                    Ok(permit) => {
                        let broker = broker.clone();
                        connections.spawn(async move {
                            let outcome = broker.serve_connection(stream).await;
                            drop(permit);
                            outcome
                        });
                    }
                    Err(_) => tracing::warn!("broker connection capacity reached"),
                }
            }
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = joined {
                    tracing::warn!(%error, "broker connection task failed");
                }
            }
        }
    }
    drop(listener);
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    drop(cleanup);
    Ok(())
}

#[cfg(unix)]
struct UnixSocketCleanup(PathBuf);

#[cfg(unix)]
impl Drop for UnixSocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(windows)]
async fn serve(endpoint: PathBuf, broker: Broker) -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = endpoint.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "named pipe endpoint is not valid UTF-8",
        )
    })?;
    let mut server = named_pipe_server(endpoint, true)?;
    print_readiness(Path::new(endpoint))?;
    let mut connections = JoinSet::new();
    let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                break;
            }
            connected = server.connect() => {
                connected?;
                let next = named_pipe_server(endpoint, false)?;
                let stream = std::mem::replace(&mut server, next);
                match permits.clone().try_acquire_owned() {
                    Ok(permit) => {
                        let broker = broker.clone();
                        connections.spawn(async move {
                            let outcome = broker.serve_connection(stream).await;
                            drop(permit);
                            outcome
                        });
                    }
                    Err(_) => tracing::warn!("broker connection capacity reached"),
                }
            }
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = joined {
                    tracing::warn!(%error, "broker connection task failed");
                }
            }
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

#[cfg(windows)]
fn named_pipe_server(
    endpoint: &str,
    first_instance: bool,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
    options.reject_remote_clients(true);
    options.first_pipe_instance(first_instance);
    options.create(endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exact_token_is_read_then_only_that_file_is_removed() {
        let path =
            std::env::temp_dir().join(format!("seeed-hal-broker-token-{}", std::process::id()));
        tokio::fs::write(&path, [0x33_u8; 32]).await.unwrap();

        assert_eq!(read_and_remove_token(&path).await.unwrap(), [0x33_u8; 32]);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn invalid_token_length_is_rejected_without_deleting_the_file() {
        let path = std::env::temp_dir().join(format!(
            "seeed-hal-broker-invalid-token-{}",
            std::process::id()
        ));
        tokio::fs::write(&path, [0x44_u8; 33]).await.unwrap();

        let error = read_and_remove_token(&path).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(path.exists());
        tokio::fs::remove_file(path).await.unwrap();
    }
}
