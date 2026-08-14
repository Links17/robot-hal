use std::sync::Weak;

use bytes::Bytes;
use seeed_hal_core::{ErrorCategory, HalResult, SessionId};
use seeed_hal_serial::{ControlLines, SerialSession};
use tokio::sync::{Mutex, mpsc, oneshot, watch};

use crate::events::EventPublisher;
use crate::registry::Registry;
use crate::runtime_error;

const COMMAND_QUEUE_CAPACITY: usize = 64;

pub(crate) enum SerialCommand {
    Read {
        max_bytes: usize,
        reply: oneshot::Sender<HalResult<Bytes>>,
    },
    Write {
        bytes: Bytes,
        reply: oneshot::Sender<HalResult<()>>,
    },
    Flush {
        reply: oneshot::Sender<HalResult<()>>,
    },
    SetControlLines {
        lines: ControlLines,
        reply: oneshot::Sender<HalResult<()>>,
    },
}

impl SerialCommand {
    fn reject_closed(self) {
        match self {
            Self::Read { reply, .. } => {
                let _ = reply.send(Err(session_closed("serial.read")));
            }
            Self::Write { reply, .. } => {
                let _ = reply.send(Err(session_closed("serial.write")));
            }
            Self::Flush { reply } => {
                let _ = reply.send(Err(session_closed("serial.flush")));
            }
            Self::SetControlLines { reply, .. } => {
                let _ = reply.send(Err(session_closed("serial.set_control_lines")));
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct ActorHandle {
    command_tx: mpsc::Sender<SerialCommand>,
    shutdown_tx: watch::Sender<bool>,
    completion_rx: watch::Receiver<Option<HalResult<()>>>,
}

impl ActorHandle {
    pub(crate) fn try_enqueue(
        &self,
        command: SerialCommand,
        operation: &'static str,
    ) -> HalResult<()> {
        self.command_tx
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => runtime_error(
                    "runtime.queue.full",
                    ErrorCategory::Unavailable,
                    operation,
                    true,
                    "the bounded serial actor command queue has reached its 64-command capacity",
                ),
                mpsc::error::TrySendError::Closed(_) => session_closed(operation),
            })
    }

    pub(crate) fn request_close(&self) {
        let _ = self.shutdown_tx.send(true);
    }

    pub(crate) async fn wait_closed(&self) -> HalResult<()> {
        let mut completion = self.completion_rx.clone();
        loop {
            if let Some(result) = completion.borrow().clone() {
                return result;
            }
            if completion.changed().await.is_err() {
                return Err(runtime_error(
                    "runtime.actor.unavailable",
                    ErrorCategory::Internal,
                    "serial.close",
                    false,
                    "the serial actor exited without publishing its close result",
                ));
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct ActorMetadata {
    pub(crate) session_id: SessionId,
}

pub(crate) fn spawn_serial_actor(
    session: Box<dyn SerialSession>,
    registry: Weak<Mutex<Registry>>,
    events: EventPublisher,
    metadata: ActorMetadata,
) -> ActorHandle {
    let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (completion_tx, completion_rx) = watch::channel(None);

    tokio::spawn(run_serial_actor(
        session,
        command_rx,
        shutdown_rx,
        completion_tx,
        registry,
        events,
        metadata,
    ));

    ActorHandle {
        command_tx,
        shutdown_tx,
        completion_rx,
    }
}

async fn run_serial_actor(
    mut session: Box<dyn SerialSession>,
    mut commands: mpsc::Receiver<SerialCommand>,
    mut shutdown: watch::Receiver<bool>,
    completion: watch::Sender<Option<HalResult<()>>>,
    registry: Weak<Mutex<Registry>>,
    events: EventPublisher,
    metadata: ActorMetadata,
) {
    loop {
        if *shutdown.borrow() {
            break;
        }

        let command = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                let _ = changed;
                None
            }
            command = commands.recv() => command,
        };
        let Some(command) = command else {
            break;
        };

        if execute_command(session.as_mut(), command, &mut shutdown).await {
            break;
        }
    }

    let close_result = session.close().await;
    while let Ok(command) = commands.try_recv() {
        command.reject_closed();
    }

    if let Some(registry) = registry.upgrade() {
        registry.lock().await.finish_close(&metadata, &events);
    }
    let _ = completion.send(Some(close_result));
}

async fn execute_command(
    session: &mut dyn SerialSession,
    command: SerialCommand,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    if *shutdown.borrow() {
        command.reject_closed();
        return true;
    }

    match command {
        SerialCommand::Read { max_bytes, reply } => {
            let (result, cancelled) = tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    let _ = changed;
                    (Err(session_closed("serial.read")), true)
                }
                result = session.read(max_bytes) => (result, false),
            };
            let _ = reply.send(result);
            cancelled
        }
        SerialCommand::Write { bytes, reply } => {
            let (result, cancelled) = tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    let _ = changed;
                    (Err(session_closed("serial.write")), true)
                }
                result = session.write_all(&bytes) => (result, false),
            };
            let _ = reply.send(result);
            cancelled
        }
        SerialCommand::Flush { reply } => {
            let (result, cancelled) = tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    let _ = changed;
                    (Err(session_closed("serial.flush")), true)
                }
                result = session.flush() => (result, false),
            };
            let _ = reply.send(result);
            cancelled
        }
        SerialCommand::SetControlLines { lines, reply } => {
            let (result, cancelled) = tokio::select! {
                biased;
                changed = shutdown.changed() => {
                    let _ = changed;
                    (Err(session_closed("serial.set_control_lines")), true)
                }
                result = session.set_control_lines(lines) => (result, false),
            };
            let _ = reply.send(result);
            cancelled
        }
    }
}

fn session_closed(operation: &'static str) -> seeed_hal_core::HalError {
    runtime_error(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "the serial session is closed",
    )
}
