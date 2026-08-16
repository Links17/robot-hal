use std::time::Duration;

use prost::Message;
use seeed_hal_can::{
    CanBatchSendError, CanBusStatus, CanFilterSet, CanFrame, CanMode,
    CanOpenConfig, ReceivedCanFrame, MAX_CAN_BATCH_FRAMES,
    MAX_CAN_ERROR_CLASSES, MAX_CLASSIC_DATA_BYTES, MAX_FD_DATA_BYTES,
};
use seeed_hal_core::{
    ErrorCategory, HalError, HalResult, LeaseMode, LeaseToken, ResourceId, SessionId,
};
use seeed_hal_protocol::v1::{self, envelope};
use seeed_hal_protocol::{
    can_receive_response_from_proto, can_send_response_from_proto,
    get_can_bus_status_response_from_proto, open_can_response_from_proto,
};

use crate::connection::ExpectedResponse;
use crate::HalClient;

/// A broker-owned CAN session. Dropping the handle never leaks a native CAN
/// handle; the owning broker connection remains responsible for revocation.
#[must_use = "a remote CAN handle owns a broker session until it is explicitly closed or its client disconnects"]
pub struct RemoteCanHandle {
    client: HalClient,
    resource_id: ResourceId,
    session_id: SessionId,
    lease: LeaseToken,
    receive_profile: ReceiveProfile,
    closed: bool,
}

#[derive(Clone, Copy)]
struct ReceiveProfile {
    fd: bool,
    error_frames: bool,
    timestamps: bool,
}

impl RemoteCanHandle {
    pub(crate) fn from_response(
        client: HalClient,
        resource_id: ResourceId,
        expected_mode: LeaseMode,
        config: &CanOpenConfig,
        response: v1::OpenCanResponse,
    ) -> HalResult<Self> {
        let (session_id, lease) = open_can_response_from_proto(response, expected_mode)
            .map_err(|error| attach_resource(error, &resource_id))?;
        let (_, can_fd, error_frames, timestamps) = client.can_capabilities();
        let fd = match config {
            CanOpenConfig::Attach(expectation) => {
                expectation.mode() == Some(CanMode::Fd)
                    || (expectation.mode().is_none() && can_fd)
            }
            CanOpenConfig::Configure(config) => config.mode() == CanMode::Fd,
        };
        Ok(Self {
            client,
            resource_id,
            session_id,
            lease,
            receive_profile: ReceiveProfile {
                fd,
                error_frames,
                timestamps,
            },
            closed: false,
        })
    }

    pub async fn send(&self, frame: CanFrame) -> Result<(), CanBatchSendError> {
        self.send_batch(vec![frame]).await
    }

    pub async fn send_batch(&self, frames: Vec<CanFrame>) -> Result<(), CanBatchSendError> {
        self.ensure_open("can.send_batch").map_err(CanBatchSendError::new)?;
        if !(1..=MAX_CAN_BATCH_FRAMES).contains(&frames.len()) {
            return Err(CanBatchSendError::new(self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "can.send_batch",
                false,
                "CAN send batch must contain 1..=64 frames",
            )));
        }

        let (can_classic, can_fd, can_error_frames, _) = self.client.can_capabilities();
        let mut payload_bytes = 0_usize;
        for frame in &frames {
            frame
                .validate()
                .map_err(|error| CanBatchSendError::new(self.resource_error(error)))?;
            let supported = match frame {
                CanFrame::ClassicData { .. } | CanFrame::ClassicRemote { .. } => can_classic,
                CanFrame::FdData { .. } => can_fd,
                CanFrame::Error { .. } => can_error_frames,
            };
            if !supported {
                return Err(CanBatchSendError::new(self.local_error(
                    "runtime.protocol.capability_unsupported",
                    ErrorCategory::Conflict,
                    "can.send_batch",
                    false,
                    "the negotiated broker protocol does not advertise the frame capability",
                )));
            }
            payload_bytes = payload_bytes.checked_add(frame.data().len()).ok_or_else(|| {
                CanBatchSendError::new(self.local_error(
                    "runtime.argument.invalid",
                    ErrorCategory::InvalidArgument,
                    "can.send_batch",
                    false,
                    "CAN send payload byte count overflows usize",
                ))
            })?;
        }
        let max_write = self.client.limits().2;
        if payload_bytes > max_write {
            return Err(CanBatchSendError::new(self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "can.send_batch",
                false,
                "CAN send payload exceeds the negotiated write maximum",
            )));
        }

        let input_count = frames.len();
        let request = v1::CanSendRequest {
            session_id: self.session_id.as_str().to_owned(),
            lease: Some((&self.lease).into()),
            frames: frames.iter().map(Into::into).collect(),
        };
        let payload = envelope::Payload::CanSendRequest(request);
        self.client
            .ensure_can_payload_fits(&payload, "can.send_batch", &self.resource_id)
            .map_err(CanBatchSendError::new)?;
        let response = self
            .client
            .send(payload, ExpectedResponse::CanSend { input_count })
            .await
            .map_err(|error| CanBatchSendError::new(self.resource_error(error)))?;
        let envelope::Payload::CanSendResponse(response) = response else {
            unreachable!()
        };
        let result = can_send_response_from_proto(response, input_count).map_err(|error| {
            let error = self.resource_error(error);
            self.client.fail(error.clone());
            CanBatchSendError::new(error)
        })?;
        result.map_err(|error| {
            CanBatchSendError::backend_prefix(
                self.resource_error(error.error().clone()),
                error.committed(),
            )
        })
    }

    pub async fn receive(
        &self,
        max_frames: usize,
        timeout: Duration,
    ) -> HalResult<Vec<ReceivedCanFrame>> {
        self.ensure_open("can.receive")?;
        if !(1..=MAX_CAN_BATCH_FRAMES).contains(&max_frames) {
            return Err(self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "can.receive",
                false,
                "CAN receive maximum must be 1..=64 frames",
            ));
        }
        let timeout_ms = u64::try_from(timeout.as_millis()).map_err(|_| {
            self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "can.receive",
                false,
                "CAN receive timeout exceeds the wire range",
            )
        })?;
        let (_, max_read, _) = self.client.limits();
        let max_data_bytes = if self.receive_profile.fd {
            MAX_FD_DATA_BYTES
        } else {
            MAX_CLASSIC_DATA_BYTES
        };
        if max_frames
            .checked_mul(max_data_bytes)
            .is_none_or(|bytes| bytes > max_read)
        {
            return Err(self.local_error(
                "runtime.argument.invalid",
                ErrorCategory::InvalidArgument,
                "can.receive",
                false,
                "CAN receive payload bound exceeds the negotiated read maximum",
            ));
        }
        if maximum_receive_envelope_len(max_frames, self.receive_profile) > self.client.limits().0 {
            return Err(self.local_error(
                "runtime.protocol.frame_too_large",
                ErrorCategory::InvalidArgument,
                "can.receive",
                false,
                "CAN receive response bound exceeds the negotiated frame maximum",
            ));
        }

        let request = v1::CanReceiveRequest {
            session_id: self.session_id.as_str().to_owned(),
            lease: Some((&self.lease).into()),
            max_frames: max_frames as u32,
            timeout_ms,
        };
        let payload = envelope::Payload::CanReceiveRequest(request);
        self.client
            .ensure_can_payload_fits(&payload, "can.receive", &self.resource_id)?;
        let response = self
            .client
            .send(
                payload,
                ExpectedResponse::CanReceive {
                    max_frames,
                    max_read_bytes: max_read,
                    allow_timestamp: self.receive_profile.timestamps,
                },
            )
            .await
            .map_err(|error| self.resource_error(error))?;
        let envelope::Payload::CanReceiveResponse(response) = response else {
            unreachable!()
        };
        can_receive_response_from_proto(response, max_frames).map_err(|error| {
            let error = self.resource_error(error);
            self.client.fail(error.clone());
            error
        })
    }

    pub async fn replace_filters(&self, filters: CanFilterSet) -> HalResult<()> {
        self.ensure_open("can.replace_filters")?;
        if filters
            .as_slice()
            .iter()
            .any(|filter| filter.classes().error())
            && !self.client.can_capabilities().2
        {
            return Err(self.local_error(
                "runtime.protocol.capability_unsupported",
                ErrorCategory::Conflict,
                "can.replace_filters",
                false,
                "the negotiated broker protocol does not advertise CAN error frames",
            ));
        }
        let request = v1::ReplaceCanFiltersRequest {
            session_id: self.session_id.as_str().to_owned(),
            lease: Some((&self.lease).into()),
            filters: Some((&filters).into()),
        };
        let payload = envelope::Payload::ReplaceCanFiltersRequest(request);
        self.client.ensure_can_payload_fits(
            &payload,
            "can.replace_filters",
            &self.resource_id,
        )?;
        self.client
            .send(payload, ExpectedResponse::ReplaceCanFilters)
            .await
            .map_err(|error| self.resource_error(error))?;
        Ok(())
    }

    pub async fn bus_status(&self) -> HalResult<CanBusStatus> {
        self.ensure_open("can.status")?;
        let request = v1::GetCanBusStatusRequest {
            session_id: self.session_id.as_str().to_owned(),
            lease: Some((&self.lease).into()),
        };
        let payload = envelope::Payload::GetCanBusStatusRequest(request);
        self.client
            .ensure_can_payload_fits(&payload, "can.status", &self.resource_id)?;
        let response = self
            .client
            .send(payload, ExpectedResponse::CanBusStatus)
            .await
            .map_err(|error| self.resource_error(error))?;
        let envelope::Payload::GetCanBusStatusResponse(response) = response else {
            unreachable!()
        };
        get_can_bus_status_response_from_proto(response).map_err(|error| {
            let error = self.resource_error(error);
            self.client.fail(error.clone());
            error
        })
    }

    /// Marks this handle closed only after the broker acknowledges the exact
    /// retained session and lease. Local queue/writer rejection is retryable.
    pub async fn close(&mut self) -> HalResult<()> {
        self.ensure_open("can.close")?;
        let request = v1::CloseSessionRequest {
            session_id: self.session_id.as_str().to_owned(),
            lease: Some((&self.lease).into()),
        };
        let payload = envelope::Payload::CloseSessionRequest(request);
        self.client
            .ensure_can_payload_fits(&payload, "can.close", &self.resource_id)?;
        self.client
            .send(payload, ExpectedResponse::CloseSession)
            .await
            .map_err(|error| self.resource_error(error))?;
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
                "the remote CAN handle is closed",
            ));
        }
        Ok(())
    }

    fn resource_error(&self, error: HalError) -> HalError {
        attach_resource(error, &self.resource_id)
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
            .expect("static remote CAN error metadata is valid")
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

fn maximum_receive_envelope_len(max_frames: usize, profile: ReceiveProfile) -> usize {
    let fd = v1::CanFrame {
        id: Some(v1::CanId {
            value: 0x1fff_ffff,
            format: v1::CanIdFormat::Extended as i32,
        }),
        kind: v1::CanFrameKind::FdData as i32,
        data: vec![0; MAX_FD_DATA_BYTES],
        bitrate_switch: true,
        error_state_indicator: true,
        ..Default::default()
    };
    let error = v1::CanFrame {
        kind: v1::CanFrameKind::Error as i32,
        data: vec![0; MAX_CLASSIC_DATA_BYTES],
        error_classes: (1..=MAX_CAN_ERROR_CLASSES as i32).collect::<Vec<_>>(),
        ..Default::default()
    };
    let classic = v1::CanFrame {
        id: Some(v1::CanId {
            value: 0x1fff_ffff,
            format: v1::CanIdFormat::Extended as i32,
        }),
        kind: v1::CanFrameKind::ClassicData as i32,
        data: vec![0; MAX_CLASSIC_DATA_BYTES],
        ..Default::default()
    };
    let frame = if profile.fd {
        fd
    } else if profile.error_frames && error.encoded_len() > classic.encoded_len() {
        error
    } else {
        classic
    };
    let timestamp = profile.timestamps.then(|| v1::CanTimestamp {
        timestamp_ns: u64::MAX,
        source: v1::CanTimestampSource::Hardware as i32,
        clock_domain: "x".repeat(255),
    });
    let received = v1::ReceivedCanFrame {
        frame: Some(frame),
        timestamp,
    };
    let response = v1::CanReceiveResponse {
        frames: std::iter::repeat_n(received, max_frames).collect(),
    };
    let envelope = v1::Envelope {
        request_id: u64::MAX,
        payload: Some(envelope::Payload::CanReceiveResponse(response)),
    };
    envelope.encoded_len()
}
