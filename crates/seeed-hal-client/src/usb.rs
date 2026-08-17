use std::time::Duration;

use bytes::Bytes;
use seeed_hal_core::{ErrorCategory, HalError, HalResult, LeaseToken, ResourceId, SessionId};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_protocol::{open_usb_response_from_proto, usb_transfer_response_from_proto};
use seeed_hal_usb::UsbTransfer;

use crate::HalClient;
use crate::connection::ExpectedResponse;

/// An opaque broker-owned USB interface session.
#[must_use = "a remote USB handle owns a broker session until explicitly closed"]
pub struct RemoteUsbHandle {
    client: HalClient,
    resource_id: ResourceId,
    session_id: SessionId,
    lease: LeaseToken,
    closed: bool,
}

impl RemoteUsbHandle {
    pub(crate) fn from_response(
        client: HalClient,
        resource_id: ResourceId,
        response: v1::OpenUsbResponse,
    ) -> HalResult<Self> {
        let (session_id, lease) = open_usb_response_from_proto(response)
            .map_err(|error| attach_resource(error, &resource_id))?;
        Ok(Self {
            client,
            resource_id,
            session_id,
            lease,
            closed: false,
        })
    }

    pub async fn transfer(&self, transfer: UsbTransfer, timeout: Duration) -> HalResult<Bytes> {
        self.ensure_open("usb.transfer")?;
        transfer
            .validate()
            .map_err(|error| attach_resource(error, &self.resource_id))?;
        let timeout_ms = u64::try_from(timeout.as_millis()).map_err(|_| {
            self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "usb.transfer",
                false,
                "USB transfer timeout exceeds the wire range",
            )
        })?;
        if timeout_ms == 0 {
            return Err(self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "usb.transfer",
                false,
                "USB transfer timeout must be non-zero",
            ));
        }
        self.client
            .require_usb_transfer("usb.transfer", &self.resource_id, &transfer)?;
        let request = transfer_to_proto(&self.session_id, &self.lease, transfer, timeout_ms);
        let payload = envelope::Payload::UsbTransferRequest(request);
        self.client
            .ensure_payload_for_resource(&payload, "usb.transfer", &self.resource_id)?;
        let payload = self
            .client
            .send(
                payload,
                ExpectedResponse::UsbTransfer {
                    max_read_bytes: self.client.limits().1,
                    resource_id: self.resource_id.clone(),
                },
            )
            .await
            .map_err(|error| attach_resource(error, &self.resource_id))?;
        let envelope::Payload::UsbTransferResponse(response) = payload else {
            unreachable!()
        };
        usb_transfer_response_from_proto(response).map_err(|error| {
            let error = attach_resource(error, &self.resource_id);
            self.client.fail(error.clone());
            error
        })
    }

    pub async fn close(&mut self) -> HalResult<()> {
        self.ensure_open("usb.close")?;
        let payload = envelope::Payload::CloseUsbRequest(v1::CloseUsbRequest {
            session_id: self.session_id.as_str().to_owned(),
            lease: Some((&self.lease).into()),
        });
        self.client
            .ensure_payload_for_resource(&payload, "usb.close", &self.resource_id)?;
        self.client
            .send(
                payload,
                ExpectedResponse::CloseUsb {
                    resource_id: self.resource_id.clone(),
                },
            )
            .await
            .map_err(|error| attach_resource(error, &self.resource_id))?;
        self.closed = true;
        Ok(())
    }

    fn ensure_open(&self, operation: &'static str) -> HalResult<()> {
        if self.closed {
            return Err(self.local_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                operation,
                false,
                "the remote USB handle is closed",
            ));
        }
        Ok(())
    }

    fn local_error(
        &self,
        name: &'static str,
        category: ErrorCategory,
        operation: &'static str,
        retryable: bool,
        message: &'static str,
    ) -> HalError {
        HalError::new(name, category, operation, retryable, message)
            .expect("static USB client error metadata is valid")
            .with_resource_id(self.resource_id.clone())
    }
}

fn transfer_to_proto(
    session_id: &SessionId,
    lease: &LeaseToken,
    transfer: UsbTransfer,
    timeout_ms: u64,
) -> v1::UsbTransferRequest {
    let mut request = v1::UsbTransferRequest {
        session_id: session_id.as_str().to_owned(),
        lease: Some(lease.into()),
        timeout_ms,
        ..Default::default()
    };
    match transfer {
        UsbTransfer::ControlOut {
            request_type,
            request: code,
            value,
            index,
            data,
        } => {
            request.kind = v1::UsbTransferKind::ControlOut as i32;
            request.request_type = u32::from(request_type);
            request.request = u32::from(code);
            request.value = u32::from(value);
            request.index = u32::from(index);
            request.data = data.to_vec();
        }
        UsbTransfer::ControlIn {
            request_type,
            request: code,
            value,
            index,
            max_bytes,
        } => {
            request.kind = v1::UsbTransferKind::ControlIn as i32;
            request.request_type = u32::from(request_type);
            request.request = u32::from(code);
            request.value = u32::from(value);
            request.index = u32::from(index);
            request.max_bytes = max_bytes as u32;
        }
        UsbTransfer::BulkOut { endpoint, data } => {
            request.kind = v1::UsbTransferKind::BulkOut as i32;
            request.endpoint = u32::from(endpoint);
            request.data = data.to_vec();
        }
        UsbTransfer::BulkIn {
            endpoint,
            max_bytes,
        } => {
            request.kind = v1::UsbTransferKind::BulkIn as i32;
            request.endpoint = u32::from(endpoint);
            request.max_bytes = max_bytes as u32;
        }
        UsbTransfer::InterruptOut { endpoint, data } => {
            request.kind = v1::UsbTransferKind::InterruptOut as i32;
            request.endpoint = u32::from(endpoint);
            request.data = data.to_vec();
        }
        UsbTransfer::InterruptIn {
            endpoint,
            max_bytes,
        } => {
            request.kind = v1::UsbTransferKind::InterruptIn as i32;
            request.endpoint = u32::from(endpoint);
            request.max_bytes = max_bytes as u32;
        }
    }
    request
}

fn attach_resource(error: HalError, resource_id: &ResourceId) -> HalError {
    if error.resource_id().is_some() {
        error
    } else {
        error.with_resource_id(resource_id.clone())
    }
}
