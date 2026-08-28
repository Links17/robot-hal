use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tokio::net::UnixListener;
use uuid::Uuid;

use crate::{Broker, ConnectionOutcome};

pub struct UnixBroker {
    broker: Broker,
    listener: UnixListener,
    socket: UnixSocketCleanup,
}

impl UnixBroker {
    pub async fn bind(broker: Broker, private_directory: impl AsRef<Path>) -> io::Result<Self> {
        let private_directory = private_directory.as_ref();
        tokio::fs::create_dir_all(private_directory).await?;
        tokio::fs::set_permissions(private_directory, std::fs::Permissions::from_mode(0o700))
            .await?;
        let socket_path = private_directory.join(format!("robot-hal-{}.sock", Uuid::new_v4()));
        let listener = UnixListener::bind(&socket_path)?;
        let socket = UnixSocketCleanup(socket_path);
        tokio::fs::set_permissions(&socket.0, std::fs::Permissions::from_mode(0o600)).await?;
        Ok(Self {
            broker,
            listener,
            socket,
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket.0
    }

    pub async fn serve_one(&self) -> io::Result<ConnectionOutcome> {
        let (stream, _) = self.listener.accept().await?;
        Ok(self.broker.serve_connection(stream).await)
    }
}

struct UnixSocketCleanup(PathBuf);

impl Drop for UnixSocketCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
