use bytes::Bytes;
use prost::Message;
use seeed_hal_core::{ErrorCategory, HalError, HalResult, LeaseToken, ResourceId, SessionId};
use seeed_hal_protocol::parse_session_lease;
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_serial::ControlLines;

use crate::HalClient;
use crate::connection::ExpectedResponse;

/// A broker-owned Serial session. Explicit `close` is the only operation that
/// sends a close request. Dropping this value never creates a runtime or
/// spawns a task; the broker remains authoritative and revokes the session
/// when the owning client connection closes.
#[must_use = "a remote serial handle owns a broker session until it is explicitly closed or its client disconnects"]
pub struct RemoteSerialHandle {
    client: HalClient,
    resource_id: ResourceId,
    session_id: SessionId,
    lease: LeaseToken,
    closed: bool,
}

impl RemoteSerialHandle {
    pub(crate) fn from_response(
        client: HalClient,
        resource_id: ResourceId,
        response: v1::OpenSerialResponse,
    ) -> HalResult<Self> {
        let (session_id, lease) = parse_session_lease(response.session_id, response.lease)
            .map_err(|error| error.with_resource_id(resource_id.clone()))?;
        Ok(Self {
            client,
            resource_id,
            session_id,
            lease,
            closed: false,
        })
    }

    fn ensure_open(&self, operation: &'static str) -> HalResult<()> {
        if self.closed {
            return Err(client_session_closed(operation, self.resource_id.clone()));
        }
        Ok(())
    }

    fn resource_error(&self, error: HalError) -> HalError {
        if error.resource_id().is_some() {
            error
        } else {
            error.with_resource_id(self.resource_id.clone())
        }
    }

    pub async fn read(&self, max_bytes: usize) -> HalResult<Bytes> {
        self.ensure_open("serial.read")?;
        let (_, max_read, _) = self.client.limits();
        if max_bytes == 0 || max_bytes > max_read || max_bytes > u32::MAX as usize {
            return Err(self.resource_error(limit_error(
                "serial.read",
                "read size exceeds the negotiated maximum",
            )));
        }
        let payload = self
            .client
            .send(
                envelope::Payload::SerialReadRequest(v1::SerialReadRequest {
                    session_id: self.session_id.as_str().to_owned(),
                    lease: Some((&self.lease).into()),
                    max_bytes: max_bytes as u32,
                }),
                ExpectedResponse::SerialRead { max_bytes },
            )
            .await
            .map_err(|error| self.resource_error(error))?;
        let envelope::Payload::SerialReadResponse(response) = payload else {
            unreachable!()
        };
        if response.data.len() > max_bytes || response.data.len() > max_read {
            let error = self.resource_error(limit_error(
                "serial.read",
                "response exceeds the requested read size",
            ));
            self.client.fail(error.clone());
            return Err(error);
        }
        Ok(Bytes::from(response.data))
    }

    pub async fn write(&self, bytes: Bytes) -> HalResult<()> {
        self.ensure_open("serial.write")?;
        let (_, _, max_write) = self.client.limits();
        if bytes.len() > max_write {
            return Err(self.resource_error(limit_error(
                "serial.write",
                "write size exceeds the negotiated maximum",
            )));
        }
        let mut request = v1::SerialWriteRequest {
            session_id: self.session_id.as_str().to_owned(),
            lease: Some((&self.lease).into()),
            data: Vec::new(),
        };
        let frame_limit = self.client.limits().0;
        if serial_write_envelope_len(&request, bytes.len()) > frame_limit {
            return Err(self.resource_error(limit_error(
                "serial.write",
                "write envelope exceeds the negotiated frame maximum",
            )));
        }
        request.data = bytes.to_vec();
        self.client
            .send(
                envelope::Payload::SerialWriteRequest(request),
                ExpectedResponse::SerialWrite,
            )
            .await
            .map_err(|error| self.resource_error(error))?;
        Ok(())
    }

    pub async fn flush(&self) -> HalResult<()> {
        self.ensure_open("serial.flush")?;
        self.client
            .send(
                envelope::Payload::SerialFlushRequest(v1::SerialFlushRequest {
                    session_id: self.session_id.as_str().to_owned(),
                    lease: Some((&self.lease).into()),
                }),
                ExpectedResponse::SerialFlush,
            )
            .await
            .map_err(|error| self.resource_error(error))?;
        Ok(())
    }

    pub async fn set_control_lines(&self, lines: ControlLines) -> HalResult<()> {
        self.ensure_open("serial.set_control_lines")?;
        self.client
            .send(
                envelope::Payload::SetSerialControlLinesRequest(v1::SetSerialControlLinesRequest {
                    session_id: self.session_id.as_str().to_owned(),
                    lease: Some((&self.lease).into()),
                    data_terminal_ready: lines.data_terminal_ready,
                    request_to_send: lines.request_to_send,
                }),
                ExpectedResponse::SetControlLines,
            )
            .await
            .map_err(|error| self.resource_error(error))?;
        Ok(())
    }

    /// Close is broker-idempotent for the retained session/lease replay
    /// window. Local retryable admission failures leave this handle open so
    /// the caller can retry. After a successful response, every operation on
    /// this handle fails locally with `runtime.session.closed`.
    pub async fn close(&mut self) -> HalResult<()> {
        self.ensure_open("serial.close")?;
        self.client
            .send(
                envelope::Payload::CloseSessionRequest(v1::CloseSessionRequest {
                    session_id: self.session_id.as_str().to_owned(),
                    lease: Some((&self.lease).into()),
                }),
                ExpectedResponse::CloseSession,
            )
            .await
            .map_err(|error| self.resource_error(error))?;
        self.closed = true;
        Ok(())
    }
}

fn client_session_closed(operation: &'static str, resource_id: ResourceId) -> HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "the remote serial handle is closed",
    )
    .expect("static remote serial handle error metadata is valid")
    .with_resource_id(resource_id)
}

fn serial_write_envelope_len(request: &v1::SerialWriteRequest, data_len: usize) -> usize {
    let data_field_len = if data_len == 0 {
        0
    } else {
        1 + varint_len(data_len as u64) + data_len
    };
    let request_len = request.encoded_len() + data_field_len;
    // request_id uses one tag byte plus at most ten value bytes. Payload field
    // 26 uses a two-byte key followed by its length-delimited message.
    11 + 2 + varint_len(request_len as u64) + request_len
}

fn varint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

fn limit_error(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "runtime.argument.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static client limit error metadata is valid")
}
