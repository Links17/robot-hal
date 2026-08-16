//! Test-only process seam for language-client integration tests.

use seeed_hal_broker::{Broker, StartupToken};
use seeed_hal_runtime::HalRuntime;
use seeed_hal_testkit::{VirtualCanAdapter, VirtualSerialAdapter};
use zeroize::Zeroize;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("virtual broker test support: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let token_path = std::env::var_os("SEEED_HAL_TEST_TOKEN_FILE")
        .ok_or("SEEED_HAL_TEST_TOKEN_FILE is required")?;
    let mut token_bytes = tokio::fs::read(&token_path).await?;
    if token_bytes.len() != 32 {
        token_bytes.zeroize();
        return Err("test token must contain exactly 32 bytes".into());
    }
    let mut token = [0_u8; 32];
    token.copy_from_slice(&token_bytes);
    token_bytes.zeroize();
    tokio::fs::remove_file(&token_path).await?;

    let startup_token = StartupToken::from_bytes(token);
    token.zeroize();
    let runtime = HalRuntime::builder()
        .serial_adapter(VirtualSerialAdapter::loopback("serial:virtual:python"))
        .can_adapter(VirtualCanAdapter::loopback("can:virtual:python"))
        .build();
    let broker = Broker::with_startup_token(runtime, startup_token);
    serve_one(broker).await
}

#[cfg(unix)]
async fn serve_one(broker: Broker) -> Result<(), Box<dyn std::error::Error>> {
    use seeed_hal_broker::listener::UnixBroker;

    let directory = std::env::var_os("SEEED_HAL_TEST_DIRECTORY")
        .ok_or("SEEED_HAL_TEST_DIRECTORY is required")?;
    let listener = UnixBroker::bind(broker, directory).await?;
    println!(
        "{}",
        serde_json::json!({ "endpoint": listener.socket_path() })
    );
    let outcome = listener.serve_one().await?;
    if let Some(error) = outcome.cleanup_error() {
        return Err(error.to_string().into());
    }
    Ok(())
}

#[cfg(windows)]
async fn serve_one(broker: Broker) -> Result<(), Box<dyn std::error::Error>> {
    use seeed_hal_broker::listener::WindowsBroker;

    let listener = WindowsBroker::bind(broker)?;
    println!(
        "{}",
        serde_json::json!({ "endpoint": listener.pipe_name() })
    );
    let outcome = listener.serve_one().await?;
    if let Some(error) = outcome.cleanup_error() {
        return Err(error.to_string().into());
    }
    Ok(())
}
