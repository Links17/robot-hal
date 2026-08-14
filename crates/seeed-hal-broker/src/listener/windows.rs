use std::io;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::{Broker, ConnectionOutcome};

pub struct WindowsBroker {
    broker: Broker,
    pipe_name: String,
    server: Mutex<NamedPipeServer>,
}

impl WindowsBroker {
    pub fn bind(broker: Broker) -> io::Result<Self> {
        let pipe_name = format!(r"\\.\pipe\seeed-hal-{}", Uuid::new_v4());
        let server = options(true).create(&pipe_name)?;
        Ok(Self {
            broker,
            pipe_name,
            server: Mutex::new(server),
        })
    }

    pub fn pipe_name(&self) -> &str {
        &self.pipe_name
    }

    pub async fn serve_one(&self) -> io::Result<ConnectionOutcome> {
        let mut server = self.server.lock().await;
        server.connect().await?;
        let next = options(false).create(&self.pipe_name)?;
        let connected = std::mem::replace(&mut *server, next);
        drop(server);
        Ok(self.broker.serve_connection(connected).await)
    }
}

fn options(first_instance: bool) -> ServerOptions {
    let mut options = ServerOptions::new();
    options.reject_remote_clients(true);
    options.first_pipe_instance(first_instance);
    options
}
