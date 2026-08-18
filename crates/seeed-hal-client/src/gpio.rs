use std::time::Duration;

use seeed_hal_core::{ErrorCategory, HalError, HalResult, LeaseToken, ResourceId, SessionId};
use seeed_hal_gpio::{GpioEdgeEvent, GpioEdgeRequest, MAX_GPIO_EVENTS};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_protocol::{
    gpio_next_edge_response_from_proto, gpio_read_response_from_proto,
    open_gpio_response_from_proto,
};

use crate::HalClient;
use crate::connection::ExpectedResponse;

/// An opaque broker-owned GPIO line-group session.
#[must_use = "a remote GPIO handle owns a broker session until explicitly closed"]
pub struct RemoteGpioHandle {
    client: HalClient,
    resource_id: ResourceId,
    session_id: SessionId,
    lease: LeaseToken,
    line_count: usize,
    closed: bool,
}

impl RemoteGpioHandle {
    pub(crate) fn from_response(
        client: HalClient,
        resource_id: ResourceId,
        line_count: usize,
        response: v1::OpenGpioResponse,
    ) -> HalResult<Self> {
        let (session_id, lease) = open_gpio_response_from_proto(response)
            .map_err(|error| attach_resource(error, &resource_id))?;
        Ok(Self {
            client,
            resource_id,
            session_id,
            lease,
            line_count,
            closed: false,
        })
    }

    pub async fn read(&self) -> HalResult<Vec<bool>> {
        self.ensure_open("gpio.read")?;
        let payload = self
            .client
            .send(
                envelope::Payload::GpioReadRequest(v1::GpioReadRequest {
                    session_id: self.session_id.as_str().to_owned(),
                    lease: Some((&self.lease).into()),
                }),
                ExpectedResponse::GpioRead {
                    line_count: self.line_count,
                    resource_id: self.resource_id.clone(),
                },
            )
            .await
            .map_err(|error| attach_resource(error, &self.resource_id))?;
        let envelope::Payload::GpioReadResponse(response) = payload else {
            unreachable!()
        };
        gpio_read_response_from_proto(response)
            .and_then(|values| {
                if values.len() == self.line_count {
                    Ok(values)
                } else {
                    Err(self.local_error(
                        "runtime.protocol.invalid_message",
                        ErrorCategory::InvalidArgument,
                        "gpio.read",
                        false,
                        "GPIO read response length does not match opened lines",
                    ))
                }
            })
            .inspect_err(|error| {
                self.client.fail(error.clone());
            })
    }

    pub async fn write(&self, values: Vec<bool>) -> HalResult<()> {
        self.ensure_open("gpio.write")?;
        if values.len() != self.line_count || values.len() > MAX_GPIO_EVENTS {
            return Err(self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "gpio.write",
                false,
                "GPIO write values must match opened lines",
            ));
        }
        let payload = envelope::Payload::GpioWriteRequest(v1::GpioWriteRequest {
            session_id: self.session_id.as_str().to_owned(),
            lease: Some((&self.lease).into()),
            values,
        });
        self.client
            .ensure_payload_for_resource(&payload, "gpio.write", &self.resource_id)?;
        self.client
            .send(
                payload,
                ExpectedResponse::GpioWrite {
                    resource_id: self.resource_id.clone(),
                },
            )
            .await
            .map_err(|error| attach_resource(error, &self.resource_id))?;
        Ok(())
    }

    pub async fn next_edge(
        &self,
        request: GpioEdgeRequest,
        timeout: Duration,
    ) -> HalResult<Option<GpioEdgeEvent>> {
        self.ensure_open("gpio.next_edge")?;
        let timeout_ms = u64::try_from(timeout.as_millis()).map_err(|_| {
            self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "gpio.next_edge",
                false,
                "GPIO edge timeout exceeds the wire range",
            )
        })?;
        if timeout_ms == 0 {
            return Err(self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "gpio.next_edge",
                false,
                "GPIO edge timeout must be non-zero",
            ));
        }
        self.client
            .require_gpio_edges("gpio.next_edge", &self.resource_id)?;
        let payload = envelope::Payload::GpioNextEdgeRequest(v1::GpioNextEdgeRequest {
            session_id: self.session_id.as_str().to_owned(),
            lease: Some((&self.lease).into()),
            rising: request.edges().contains(seeed_hal_gpio::GpioEdge::Rising),
            falling: request.edges().contains(seeed_hal_gpio::GpioEdge::Falling),
            capacity: request.capacity() as u32,
            timeout_ms,
        });
        self.client
            .ensure_payload_for_resource(&payload, "gpio.next_edge", &self.resource_id)?;
        let payload = self
            .client
            .send(
                payload,
                ExpectedResponse::GpioNextEdge {
                    resource_id: self.resource_id.clone(),
                },
            )
            .await
            .map_err(|error| attach_resource(error, &self.resource_id))?;
        let envelope::Payload::GpioNextEdgeResponse(response) = payload else {
            unreachable!()
        };
        gpio_next_edge_response_from_proto(response).map_err(|error| {
            let error = attach_resource(error, &self.resource_id);
            self.client.fail(error.clone());
            error
        })
    }

    pub async fn close(&mut self) -> HalResult<()> {
        self.ensure_open("gpio.close")?;
        let payload = envelope::Payload::CloseGpioRequest(v1::CloseGpioRequest {
            session_id: self.session_id.as_str().to_owned(),
            lease: Some((&self.lease).into()),
        });
        self.client
            .send(
                payload,
                ExpectedResponse::CloseGpio {
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
            Err(self.local_error(
                "runtime.session.closed",
                ErrorCategory::Conflict,
                operation,
                false,
                "the remote GPIO handle is closed",
            ))
        } else {
            Ok(())
        }
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
            .expect("static GPIO client error metadata is valid")
            .with_resource_id(self.resource_id.clone())
    }
}

fn attach_resource(error: HalError, resource_id: &ResourceId) -> HalError {
    if error.resource_id().is_some() {
        error
    } else {
        error.with_resource_id(resource_id.clone())
    }
}
