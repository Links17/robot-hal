#![forbid(unsafe_code)]

mod manifest;
mod token;

use std::io;
use std::path::{Path, PathBuf};

use clap::{Parser, ValueEnum};
#[cfg(all(
    feature = "avfoundation",
    target_os = "macos",
    not(feature = "virtual-adapters")
))]
use robot_hal_adapter_avfoundation::AvFoundationAdapter;
#[cfg(all(
    feature = "linux-gpio",
    target_os = "linux",
    not(feature = "virtual-adapters")
))]
use robot_hal_adapter_linux_gpio::LinuxGpioAdapter;
#[cfg(all(
    feature = "mediafoundation",
    windows,
    not(feature = "virtual-adapters")
))]
use robot_hal_adapter_mediafoundation::MediaFoundationAdapter;
#[cfg(all(feature = "nusb", not(feature = "virtual-adapters")))]
use robot_hal_adapter_nusb::NusbAdapter;
#[cfg(feature = "pcan")]
use robot_hal_adapter_pcan::PcanAdapter;
#[cfg(feature = "serialport")]
use robot_hal_adapter_serialport::SerialPortAdapter;
#[cfg(feature = "socketcan")]
use robot_hal_adapter_socketcan::SocketCanAdapter;
#[cfg(all(
    feature = "v4l2",
    target_os = "linux",
    not(feature = "virtual-adapters")
))]
use robot_hal_adapter_v4l2::V4l2Adapter;
#[cfg(all(feature = "windows-gpio", windows, not(feature = "virtual-adapters")))]
use robot_hal_adapter_windows_gpio::WindowsGpioAdapter;
use robot_hal_broker::Broker;
use robot_hal_runtime::HalRuntime;
use serde::Serialize;
use tokio::task::JoinSet;

#[cfg(feature = "virtual-adapters")]
use robot_hal_testkit::{
    VirtualCameraAdapter, VirtualCanAdapter, VirtualGpioAdapter, VirtualSerialAdapter,
    VirtualUsbAdapter,
};

const MAX_CONNECTIONS: usize = 64;

#[derive(Default)]
struct ServeMetrics {
    max_retained_tasks: usize,
}

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
    #[arg(long, value_enum)]
    require_adapter: Vec<RequiredAdapter>,
}

#[derive(Clone, Copy, ValueEnum)]
enum LogFormat {
    Json,
    Pretty,
}

#[derive(Clone, Copy, Eq, PartialEq, ValueEnum)]
enum RequiredAdapter {
    Pcan,
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
            eprintln!("robot-hal-broker: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.manifest {
        let manifest = tokio::task::spawn_blocking(manifest::BrokerManifest::current).await??;
        println!("{}", serde_json::to_string(&manifest)?);
        return Ok(());
    }

    install_tracing(args.log_format)?;
    let endpoint = args.endpoint.expect("clap requires --endpoint");
    let token_path = args
        .auth_token_file
        .expect("clap requires --auth-token-file");
    let token = token::read_and_remove_token(token_path).await?;
    let runtime = build_runtime(&args.require_adapter)?;
    let broker = Broker::with_startup_token(runtime, token);
    serve(endpoint, broker).await?;
    Ok(())
}

fn build_runtime(required: &[RequiredAdapter]) -> Result<HalRuntime, Box<dyn std::error::Error>> {
    #[allow(unused_mut)]
    let mut builder = HalRuntime::builder();
    #[cfg(feature = "serialport")]
    {
        builder = builder.serial_adapter(SerialPortAdapter::new());
    }
    #[cfg(feature = "socketcan")]
    {
        builder = builder.can_adapter(SocketCanAdapter::new());
    }
    #[cfg(all(feature = "nusb", not(feature = "virtual-adapters")))]
    {
        builder = builder.usb_adapter(NusbAdapter::new());
    }
    #[cfg(all(
        feature = "linux-gpio",
        target_os = "linux",
        not(feature = "virtual-adapters")
    ))]
    {
        builder = builder.gpio_adapter(LinuxGpioAdapter::new());
    }
    #[cfg(all(feature = "windows-gpio", windows, not(feature = "virtual-adapters")))]
    {
        builder = builder.gpio_adapter(WindowsGpioAdapter::new());
    }
    #[cfg(all(
        feature = "avfoundation",
        target_os = "macos",
        not(feature = "virtual-adapters")
    ))]
    {
        builder = builder.camera_adapter(AvFoundationAdapter::new());
    }
    #[cfg(all(
        feature = "v4l2",
        target_os = "linux",
        not(feature = "virtual-adapters")
    ))]
    {
        builder = builder.camera_adapter(V4l2Adapter::new());
    }
    #[cfg(all(
        feature = "mediafoundation",
        windows,
        not(feature = "virtual-adapters")
    ))]
    {
        builder = builder.camera_adapter(MediaFoundationAdapter::new());
    }
    #[cfg(feature = "virtual-adapters")]
    {
        builder = builder
            .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:broker-app"))
            .can_adapter(VirtualCanAdapter::loopback(
                "can:virtual:broker-app:classic",
            ))
            .can_adapter(VirtualCanAdapter::loopback_fd("can:virtual:broker-app:fd"))
            .usb_adapter(VirtualUsbAdapter::loopback("usb:virtual:broker-app"))
            .gpio_adapter(VirtualGpioAdapter::line_bank("gpio:virtual:broker-app", 2))
            .camera_adapter(VirtualCameraAdapter::pattern("camera:virtual:broker-app"));
    }
    #[cfg(feature = "pcan")]
    {
        match PcanAdapter::load() {
            Ok(adapter) => builder = builder.can_adapter(adapter),
            Err(error) => {
                log_adapter_load_error("pcan", &error);
                if required.contains(&RequiredAdapter::Pcan) {
                    return Err(Box::new(error));
                }
            }
        }
    }
    #[cfg(not(feature = "pcan"))]
    if required.contains(&RequiredAdapter::Pcan) {
        return Err("the broker was built without the pcan adapter feature".into());
    }
    Ok(builder.build())
}

#[cfg(feature = "pcan")]
fn log_adapter_load_error(adapter: &'static str, error: &robot_hal_core::HalError) {
    tracing::warn!(
        adapter,
        error.name = error.name().as_str(),
        error.category = ?error.category(),
        error.operation = error.operation().as_str(),
        error.retryable = error.retryable(),
        "optional CAN adapter unavailable",
    );
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
    serve_unix_until(
        endpoint,
        broker,
        async { tokio::signal::ctrl_c().await },
        MAX_CONNECTIONS,
    )
    .await?;
    Ok(())
}

#[cfg(unix)]
async fn serve_unix_until<F>(
    endpoint: PathBuf,
    broker: Broker,
    shutdown: F,
    max_connections: usize,
) -> Result<ServeMetrics, Box<dyn std::error::Error>>
where
    F: std::future::Future<Output = io::Result<()>>,
{
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
    let (listener, cleanup) = bind_unix_socket(endpoint.clone())?;
    tokio::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).await?;
    print_readiness(&endpoint)?;

    let (connection_shutdown, _) = tokio::sync::watch::channel(false);
    let mut connections = JoinSet::new();
    let mut metrics = ServeMetrics::default();
    tokio::pin!(shutdown);
    let serve_result = loop {
        tokio::select! {
            signal = &mut shutdown => {
                break signal;
            }
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(error),
                };
                reap_finished(&mut connections);
                if connections.len() >= max_connections {
                    tracing::warn!("broker connection capacity reached");
                    continue;
                }
                let broker = broker.clone();
                let mut shutdown = connection_shutdown.subscribe();
                connections.spawn(async move {
                    broker
                        .serve_connection_until(stream, async move {
                            wait_for_shutdown(&mut shutdown).await;
                        })
                        .await
                });
                metrics.max_retained_tasks = metrics.max_retained_tasks.max(connections.len());
            }
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                log_connection_result(joined);
            }
        }
    };
    drop(listener);
    connection_shutdown.send_replace(true);
    while let Some(joined) = connections.join_next().await {
        log_connection_result(joined);
    }
    drop(cleanup);
    serve_result?;
    Ok(metrics)
}

async fn wait_for_shutdown(shutdown: &mut tokio::sync::watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

fn reap_finished(connections: &mut JoinSet<robot_hal_broker::ConnectionOutcome>) {
    while let Some(joined) = connections.try_join_next() {
        log_connection_result(joined);
    }
}

fn log_connection_result(
    result: Result<robot_hal_broker::ConnectionOutcome, tokio::task::JoinError>,
) {
    match result {
        Ok(outcome) => {
            if let Some(error) = outcome.connection_error() {
                log_structured_error(structured_error_fields("connection", error));
            }
            if let Some(error) = outcome.cleanup_error() {
                log_structured_error(structured_error_fields("cleanup", error));
            }
        }
        Err(error) => tracing::warn!(%error, "broker connection task failed"),
    }
}

#[derive(Debug)]
struct StructuredErrorFields<'a> {
    kind: &'static str,
    name: &'a str,
    category: robot_hal_core::ErrorCategory,
    operation: &'a str,
    retryable: bool,
}

fn structured_error_fields<'a>(
    kind: &'static str,
    error: &'a robot_hal_core::HalError,
) -> StructuredErrorFields<'a> {
    StructuredErrorFields {
        kind,
        name: error.name().as_str(),
        category: error.category(),
        operation: error.operation().as_str(),
        retryable: error.retryable(),
    }
}

fn log_structured_error(fields: StructuredErrorFields<'_>) {
    tracing::warn!(
        error.kind = fields.kind,
        error.name = fields.name,
        error.category = ?fields.category,
        error.operation = fields.operation,
        error.retryable = fields.retryable,
        "broker connection ended with a structured error",
    );
}

#[cfg(any(test, windows))]
async fn wait_for_either_shutdown_signal<C, B>(ctrl_c: C, ctrl_break: B) -> io::Result<()>
where
    C: std::future::Future<Output = io::Result<()>>,
    B: std::future::Future<Output = io::Result<()>>,
{
    tokio::pin!(ctrl_c);
    tokio::pin!(ctrl_break);
    tokio::select! {
        result = &mut ctrl_c => result,
        result = &mut ctrl_break => result,
    }
}

#[cfg(windows)]
async fn wait_for_windows_shutdown() -> io::Result<()> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    let mut ctrl_break = tokio::signal::windows::ctrl_break()?;
    wait_for_either_shutdown_signal(
        async move {
            ctrl_c.recv().await.ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "Ctrl-C signal stream closed")
            })
        },
        async move {
            ctrl_break.recv().await.ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "Ctrl-Break signal stream closed")
            })
        },
    )
    .await
}

#[cfg(test)]
mod shutdown_signal_tests {
    use super::*;

    #[tokio::test]
    async fn either_shutdown_signal_accepts_the_second_signal() {
        let ctrl_c = std::future::pending::<io::Result<()>>();
        let ctrl_break = std::future::ready(Ok(()));

        wait_for_either_shutdown_signal(ctrl_c, ctrl_break)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn either_shutdown_signal_propagates_listener_failure() {
        let ctrl_c = std::future::ready(Err(io::Error::other("listener failed")));
        let ctrl_break = std::future::pending::<io::Result<()>>();

        let error = wait_for_either_shutdown_signal(ctrl_c, ctrl_break)
            .await
            .unwrap_err();

        assert_eq!(error.to_string(), "listener failed");
    }
}

#[cfg(test)]
mod connection_logging_tests {
    use robot_hal_core::{ErrorCategory, HalError};

    use super::structured_error_fields;

    #[test]
    fn structured_connection_logging_excludes_diagnostics_and_secrets() {
        let error = HalError::new(
            "runtime.protocol.authentication_failed",
            ErrorCategory::Conflict,
            "runtime.protocol.handshake",
            false,
            "secret-token-material",
        )
        .unwrap();

        let fields = structured_error_fields("connection", &error);

        assert_eq!(fields.kind, "connection");
        assert_eq!(fields.name, "runtime.protocol.authentication_failed");
        assert_eq!(fields.operation, "runtime.protocol.handshake");
        assert_eq!(fields.category, ErrorCategory::Conflict);
        assert!(!fields.retryable);
        assert!(!format!("{fields:?}").contains("secret-token-material"));
    }
}

#[cfg(all(test, feature = "virtual-adapters"))]
mod camera_composition_tests {
    use super::build_runtime;

    #[tokio::test]
    async fn virtual_adapter_feature_registers_the_broker_camera() {
        let runtime = build_runtime(&[]).expect("virtual broker runtime builds");

        let cameras = runtime
            .enumerate_camera()
            .await
            .expect("virtual camera is registered");

        assert_eq!(cameras.len(), 1);
        assert_eq!(cameras[0].id().as_str(), "camera:virtual:broker-app");
    }
}

#[cfg(unix)]
struct UnixSocketCleanup(PathBuf);

#[cfg(unix)]
impl Drop for UnixSocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[cfg(unix)]
fn bind_unix_socket(
    endpoint: PathBuf,
) -> io::Result<(tokio::net::UnixListener, UnixSocketCleanup)> {
    let listener = tokio::net::UnixListener::bind(&endpoint)?;
    let cleanup = UnixSocketCleanup(endpoint);
    Ok((listener, cleanup))
}

#[cfg(windows)]
async fn serve(endpoint: PathBuf, broker: Broker) -> Result<(), Box<dyn std::error::Error>> {
    serve_windows_until(
        endpoint,
        broker,
        wait_for_windows_shutdown(),
        MAX_CONNECTIONS,
    )
    .await?;
    Ok(())
}

#[cfg(windows)]
async fn serve_windows_until<F>(
    endpoint: PathBuf,
    broker: Broker,
    shutdown: F,
    max_connections: usize,
) -> Result<ServeMetrics, Box<dyn std::error::Error>>
where
    F: std::future::Future<Output = io::Result<()>>,
{
    let endpoint = endpoint.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "named pipe endpoint is not valid UTF-8",
        )
    })?;
    let mut server = named_pipe_server(endpoint, true)?;
    print_readiness(Path::new(endpoint))?;
    let (connection_shutdown, _) = tokio::sync::watch::channel(false);
    let mut connections = JoinSet::new();
    let mut metrics = ServeMetrics::default();
    tokio::pin!(shutdown);
    let serve_result = loop {
        tokio::select! {
            signal = &mut shutdown => {
                break signal;
            }
            connected = server.connect() => {
                if let Err(error) = connected {
                    break Err(error);
                }
                let next = match named_pipe_server(endpoint, false) {
                    Ok(next) => next,
                    Err(error) => break Err(error),
                };
                let stream = std::mem::replace(&mut server, next);
                reap_finished(&mut connections);
                if connections.len() >= max_connections {
                    tracing::warn!("broker connection capacity reached");
                    continue;
                }
                let broker = broker.clone();
                let mut shutdown = connection_shutdown.subscribe();
                connections.spawn(async move {
                    broker
                        .serve_connection_until(stream, async move {
                            wait_for_shutdown(&mut shutdown).await;
                        })
                        .await
                });
                metrics.max_retained_tasks = metrics.max_retained_tasks.max(connections.len());
            }
            Some(joined) = connections.join_next(), if !connections.is_empty() => {
                log_connection_result(joined);
            }
        }
    };
    connection_shutdown.send_replace(true);
    while let Some(joined) = connections.join_next().await {
        log_connection_result(joined);
    }
    serve_result?;
    Ok(metrics)
}

#[cfg(windows)]
fn named_pipe_server(
    endpoint: &str,
    first_instance: bool,
) -> io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    let mut options = tokio::net::windows::named_pipe::ServerOptions::new();
    options.reject_remote_clients(true);
    options.first_pipe_instance(first_instance);
    robot_hal_windows_security::create_current_user_named_pipe(&options, endpoint)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::token::read_and_remove_token;
    use robot_hal_broker::StartupToken;

    async fn test_deadline<T>(
        future: impl std::future::Future<Output = T>,
        message: &'static str,
    ) -> T {
        tokio::time::timeout(std::time::Duration::from_secs(1), future)
            .await
            .expect(message)
    }

    #[tokio::test]
    async fn unix_socket_cleanup_is_armed_immediately_after_bind() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let endpoint = PathBuf::from(format!("/tmp/shg-{}-{nonce}.sock", std::process::id()));

        let (listener, cleanup) = bind_unix_socket(endpoint.clone()).unwrap();

        assert!(endpoint.exists());
        drop(listener);
        drop(cleanup);
        assert!(!endpoint.exists());
    }

    #[cfg(unix)]
    fn private_token_path(label: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "robot-hal-token-{label}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("token");
        (directory, path)
    }

    #[cfg(unix)]
    async fn write_private_token(path: &Path, bytes: &[u8]) {
        use std::os::unix::fs::PermissionsExt;

        tokio::fs::write(path, bytes).await.unwrap();
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .await
            .unwrap();
    }

    async fn wait_for_endpoint(endpoint: &Path) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !endpoint.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("broker endpoint must become ready before the test deadline");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn exact_token_is_read_then_only_that_file_is_removed() {
        let (directory, path) = private_token_path("exact");
        write_private_token(&path, &[0x33_u8; 32]).await;

        assert_eq!(
            read_and_remove_token(path.clone())
                .await
                .unwrap()
                .expose_bytes(),
            &[0x33_u8; 32]
        );
        assert!(!path.exists());
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn invalid_token_length_is_rejected_without_deleting_the_file() {
        let (directory, path) = private_token_path("invalid-length");
        write_private_token(&path, &[0x44_u8; 33]).await;

        let error = read_and_remove_token(path.clone())
            .await
            .err()
            .expect("invalid token length must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(path.exists());
        tokio::fs::remove_file(path).await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn token_symlink_is_rejected_without_deleting_target() {
        use std::os::unix::fs::symlink;

        let (directory, path) = private_token_path("symlink");
        let target = directory.join("target");
        write_private_token(&target, &[0x55_u8; 32]).await;
        symlink(&target, &path).unwrap();

        assert_eq!(
            read_and_remove_token(path.clone())
                .await
                .err()
                .expect("symlink token must fail")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(path.is_symlink());
        assert!(target.exists());
        tokio::fs::remove_file(path).await.unwrap();
        tokio::fs::remove_file(target).await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn publicly_readable_token_file_is_rejected_without_deletion() {
        use std::os::unix::fs::PermissionsExt;

        let (directory, path) = private_token_path("public-file");
        tokio::fs::write(&path, [0x66_u8; 32]).await.unwrap();
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .await
            .unwrap();

        assert_eq!(
            read_and_remove_token(path.clone())
                .await
                .err()
                .expect("public token mode must fail")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(path.exists());
        tokio::fs::remove_file(path).await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn token_in_public_parent_directory_is_rejected_without_deletion() {
        use std::os::unix::fs::PermissionsExt;

        let (directory, path) = private_token_path("public-parent");
        write_private_token(&path, &[0x77_u8; 32]).await;
        tokio::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755))
            .await
            .unwrap();

        assert_eq!(
            read_and_remove_token(path.clone())
                .await
                .err()
                .expect("public token parent must fail")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert!(path.exists());
        tokio::fs::remove_file(path).await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn executable_shutdown_revokes_open_owner_before_returning() {
        use robot_hal_client::{ConnectionOptions, HalClient};
        use robot_hal_core::OwnerId;
        use robot_hal_runtime::RuntimeEventKind;
        use robot_hal_serial::SerialConfig;
        use robot_hal_testkit::VirtualSerialAdapter;
        use std::os::unix::fs::PermissionsExt;

        const TOKEN: [u8; 32] = [0x91; 32];
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = PathBuf::from(format!("/tmp/shapp-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = directory.join("broker.sock");
        let runtime = HalRuntime::builder()
            .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:shutdown"))
            .build();
        let mut events = runtime.subscribe();
        let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server_endpoint = endpoint.clone();
        let server = tokio::spawn(async move {
            serve_unix_until(
                server_endpoint,
                broker,
                async move {
                    shutdown_rx.await.map_err(io::Error::other)?;
                    Ok(())
                },
                2,
            )
            .await
            .unwrap()
        });
        wait_for_endpoint(&endpoint).await;

        let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
            .await
            .unwrap();
        let descriptor = client.enumerate_serial().await.unwrap().remove(0);
        let _serial = client
            .open_serial(descriptor.selector(), SerialConfig::default())
            .await
            .unwrap();
        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("cooperative shutdown must complete")
            .unwrap();

        let mut saw_close = false;
        for _ in 0..2 {
            let event = test_deadline(events.recv(), "shutdown must publish runtime events")
                .await
                .unwrap();
            if event.kind() == RuntimeEventKind::SessionClosed {
                saw_close = true;
            }
        }
        assert!(saw_close, "shutdown must publish session closure");
        runtime
            .open_serial(
                OwnerId::parse("app-test:reuse-after-shutdown").unwrap(),
                descriptor.selector(),
                SerialConfig::default(),
            )
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
        client.close().await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn executable_shutdown_error_still_revokes_open_owner_before_returning() {
        use robot_hal_client::{ConnectionOptions, HalClient};
        use robot_hal_core::OwnerId;
        use robot_hal_serial::SerialConfig;
        use robot_hal_testkit::VirtualSerialAdapter;
        use std::os::unix::fs::PermissionsExt;

        const TOKEN: [u8; 32] = [0x93; 32];
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = PathBuf::from(format!("/tmp/sherr-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = directory.join("broker.sock");
        let runtime = HalRuntime::builder()
            .serial_adapter(VirtualSerialAdapter::loopback(
                "serial:virtual:shutdown-error",
            ))
            .build();
        let broker = Broker::with_startup_token(runtime.clone(), StartupToken::from_bytes(TOKEN));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server_endpoint = endpoint.clone();
        let server = tokio::spawn(async move {
            serve_unix_until(
                server_endpoint,
                broker,
                async move {
                    shutdown_rx.await.map_err(io::Error::other)?;
                    Err(io::Error::other("test shutdown failure"))
                },
                2,
            )
            .await
            .is_err()
        });
        wait_for_endpoint(&endpoint).await;

        let client = HalClient::connect(ConnectionOptions::new(&endpoint, TOKEN))
            .await
            .unwrap();
        let descriptor = client.enumerate_serial().await.unwrap().remove(0);
        let _serial = client
            .open_serial(descriptor.selector(), SerialConfig::default())
            .await
            .unwrap();
        shutdown_tx.send(()).unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("failed shutdown must still complete cleanup")
            .unwrap();
        assert!(result);

        runtime
            .open_serial(
                OwnerId::parse("app-test:reuse-after-shutdown-error").unwrap(),
                descriptor.selector(),
                SerialConfig::default(),
            )
            .await
            .unwrap()
            .close()
            .await
            .unwrap();
        client.close().await.unwrap();
        tokio::fs::remove_dir(directory).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn executable_connection_churn_keeps_retained_tasks_bounded() {
        use robot_hal_testkit::VirtualSerialAdapter;
        use std::os::unix::fs::PermissionsExt;

        const TOKEN: [u8; 32] = [0x92; 32];
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = PathBuf::from(format!("/tmp/shchurn-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let endpoint = directory.join("broker.sock");
        let runtime = HalRuntime::builder()
            .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:churn"))
            .build();
        let broker = Broker::with_startup_token(runtime, StartupToken::from_bytes(TOKEN));
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server_endpoint = endpoint.clone();
        let server = tokio::spawn(async move {
            serve_unix_until(
                server_endpoint,
                broker,
                async move {
                    shutdown_rx.await.map_err(io::Error::other)?;
                    Ok(())
                },
                2,
            )
            .await
            .unwrap()
        });
        wait_for_endpoint(&endpoint).await;

        for _ in 0..20 {
            let stream = tokio::net::UnixStream::connect(&endpoint).await.unwrap();
            drop(stream);
            tokio::task::yield_now().await;
        }
        shutdown_tx.send(()).unwrap();
        let metrics = tokio::time::timeout(std::time::Duration::from_secs(1), server)
            .await
            .expect("bounded churn shutdown must complete")
            .unwrap();
        assert!(metrics.max_retained_tasks <= 2);
        tokio::fs::remove_dir(directory).await.unwrap();
    }
}
