use std::time::Duration;

use seeed_hal_can::{
    CanActiveConfig, CanBatchSendError, CanBitTiming, CanBusState, CanBusStatus,
    CanConfigureConfig, CanErrorClass, CanFilter, CanFilterSet, CanFrame, CanFrameClasses, CanId,
    CanIdFormat, CanLinkExpectation, CanMode, CanOpenConfig, CanTimestamp, CanTimestampSource,
    MAX_CAN_BATCH_FRAMES, ReceivedCanFrame,
};
use seeed_hal_core::{
    HalError, HalResult, LeaseMode, LeaseToken, ResourceDescriptor, ResourceSelector, SessionId,
};

use crate::conversion::required_enum;
use crate::{error_from_proto, invalid_message, parse_session_lease, v1};

fn invalid(field: &'static str, detail: &'static str) -> HalError {
    invalid_message(format!("{field} {detail}"))
}

fn required<T>(value: Option<T>, field: &'static str) -> HalResult<T> {
    value.ok_or_else(|| invalid(field, "is required"))
}

fn mode_from_proto(value: i32, field: &'static str) -> HalResult<CanMode> {
    match required_enum::<v1::CanMode>(value, field)? {
        v1::CanMode::Classic => Ok(CanMode::Classic),
        v1::CanMode::Fd => Ok(CanMode::Fd),
        v1::CanMode::Unspecified => Err(invalid(field, "is required")),
    }
}

fn mode_to_proto(value: CanMode) -> v1::CanMode {
    match value {
        CanMode::Classic => v1::CanMode::Classic,
        CanMode::Fd => v1::CanMode::Fd,
    }
}

fn lease_mode_from_proto(value: i32, field: &'static str) -> HalResult<LeaseMode> {
    match required_enum::<v1::LeaseMode>(value, field)? {
        v1::LeaseMode::Observe => Ok(LeaseMode::Observe),
        v1::LeaseMode::Control => Ok(LeaseMode::Control),
        v1::LeaseMode::Maintenance => Ok(LeaseMode::Maintenance),
        v1::LeaseMode::Unspecified => Err(invalid(field, "is required")),
    }
}

impl TryFrom<v1::CanId> for CanId {
    type Error = HalError;

    fn try_from(value: v1::CanId) -> HalResult<Self> {
        match required_enum::<v1::CanIdFormat>(value.format, "can_id.format")? {
            v1::CanIdFormat::Standard => {
                let id = u16::try_from(value.value)
                    .map_err(|_| invalid("can_id.value", "exceeds the standard ID range"))?;
                CanId::standard(id)
                    .map_err(|_| invalid("can_id.value", "exceeds the standard ID range"))
            }
            v1::CanIdFormat::Extended => CanId::extended(value.value)
                .map_err(|_| invalid("can_id.value", "exceeds the extended ID range")),
            v1::CanIdFormat::Either => Err(invalid(
                "can_id.format",
                "must be standard or extended for a frame",
            )),
            v1::CanIdFormat::Unspecified => Err(invalid("can_id.format", "is required")),
        }
    }
}

impl From<&CanId> for v1::CanId {
    fn from(value: &CanId) -> Self {
        Self {
            value: value.value(),
            format: if value.is_standard() {
                v1::CanIdFormat::Standard
            } else {
                v1::CanIdFormat::Extended
            } as i32,
        }
    }
}

fn error_class_from_proto(value: i32) -> HalResult<CanErrorClass> {
    match required_enum::<v1::CanErrorClass>(value, "can_frame.error_classes")? {
        v1::CanErrorClass::TxTimeout => Ok(CanErrorClass::TxTimeout),
        v1::CanErrorClass::LostArbitration => Ok(CanErrorClass::LostArbitration),
        v1::CanErrorClass::Controller => Ok(CanErrorClass::Controller),
        v1::CanErrorClass::Protocol => Ok(CanErrorClass::Protocol),
        v1::CanErrorClass::Transceiver => Ok(CanErrorClass::Transceiver),
        v1::CanErrorClass::NoAcknowledgement => Ok(CanErrorClass::NoAcknowledgement),
        v1::CanErrorClass::BusOff => Ok(CanErrorClass::BusOff),
        v1::CanErrorClass::BusError => Ok(CanErrorClass::BusError),
        v1::CanErrorClass::Restarted => Ok(CanErrorClass::Restarted),
        v1::CanErrorClass::Other => Ok(CanErrorClass::Other),
        v1::CanErrorClass::Unspecified => Err(invalid(
            "can_frame.error_classes",
            "contains an unspecified value",
        )),
    }
}

fn error_class_to_proto(value: CanErrorClass) -> v1::CanErrorClass {
    match value {
        CanErrorClass::TxTimeout => v1::CanErrorClass::TxTimeout,
        CanErrorClass::LostArbitration => v1::CanErrorClass::LostArbitration,
        CanErrorClass::Controller => v1::CanErrorClass::Controller,
        CanErrorClass::Protocol => v1::CanErrorClass::Protocol,
        CanErrorClass::Transceiver => v1::CanErrorClass::Transceiver,
        CanErrorClass::NoAcknowledgement => v1::CanErrorClass::NoAcknowledgement,
        CanErrorClass::BusOff => v1::CanErrorClass::BusOff,
        CanErrorClass::BusError => v1::CanErrorClass::BusError,
        CanErrorClass::Restarted => v1::CanErrorClass::Restarted,
        CanErrorClass::Other => v1::CanErrorClass::Other,
    }
}

impl TryFrom<v1::CanFrame> for CanFrame {
    type Error = HalError;

    fn try_from(value: v1::CanFrame) -> HalResult<Self> {
        let kind = required_enum::<v1::CanFrameKind>(value.kind, "can_frame.kind")?;
        match kind {
            v1::CanFrameKind::ClassicData => {
                if value.remote_dlc != 0
                    || value.bitrate_switch
                    || value.error_state_indicator
                    || !value.error_classes.is_empty()
                {
                    return Err(invalid(
                        "can_frame",
                        "has fields incompatible with classic data",
                    ));
                }
                let id = CanId::try_from(required(value.id, "can_frame.id")?)?;
                CanFrame::classic_data(id, value.data)
                    .map_err(|_| invalid("can_frame.data", "exceeds the classic payload bound"))
            }
            v1::CanFrameKind::ClassicRemote => {
                if !value.data.is_empty()
                    || value.bitrate_switch
                    || value.error_state_indicator
                    || !value.error_classes.is_empty()
                {
                    return Err(invalid(
                        "can_frame",
                        "has fields incompatible with a classic remote frame",
                    ));
                }
                let id = CanId::try_from(required(value.id, "can_frame.id")?)?;
                let dlc = u8::try_from(value.remote_dlc)
                    .map_err(|_| invalid("can_frame.remote_dlc", "exceeds the classic bound"))?;
                CanFrame::classic_remote(id, dlc)
                    .map_err(|_| invalid("can_frame.remote_dlc", "exceeds the classic bound"))
            }
            v1::CanFrameKind::FdData => {
                if value.remote_dlc != 0 || !value.error_classes.is_empty() {
                    return Err(invalid("can_frame", "has fields incompatible with FD data"));
                }
                let id = CanId::try_from(required(value.id, "can_frame.id")?)?;
                CanFrame::fd_data(
                    id,
                    value.data,
                    value.bitrate_switch,
                    value.error_state_indicator,
                )
                .map_err(|_| invalid("can_frame.data", "has an invalid FD payload length"))
            }
            v1::CanFrameKind::Error => {
                if value.id.is_some()
                    || value.remote_dlc != 0
                    || value.bitrate_switch
                    || value.error_state_indicator
                {
                    return Err(invalid(
                        "can_frame",
                        "has fields incompatible with an error frame",
                    ));
                }
                let classes = value
                    .error_classes
                    .into_iter()
                    .map(error_class_from_proto)
                    .collect::<HalResult<Vec<_>>>()?;
                CanFrame::error(classes, value.data).map_err(|_| {
                    invalid(
                        "can_frame.error_classes/data",
                        "does not form a valid error frame",
                    )
                })
            }
            v1::CanFrameKind::Unspecified => Err(invalid("can_frame.kind", "is required")),
        }
    }
}

impl From<&CanFrame> for v1::CanFrame {
    fn from(value: &CanFrame) -> Self {
        match value {
            CanFrame::ClassicData { id, data } => Self {
                id: Some(id.into()),
                kind: v1::CanFrameKind::ClassicData as i32,
                data: data.to_vec(),
                ..Default::default()
            },
            CanFrame::ClassicRemote { id, dlc } => Self {
                id: Some(id.into()),
                kind: v1::CanFrameKind::ClassicRemote as i32,
                remote_dlc: u32::from(*dlc),
                ..Default::default()
            },
            CanFrame::FdData {
                id,
                data,
                bitrate_switch,
                error_state_indicator,
            } => Self {
                id: Some(id.into()),
                kind: v1::CanFrameKind::FdData as i32,
                data: data.to_vec(),
                bitrate_switch: *bitrate_switch,
                error_state_indicator: *error_state_indicator,
                ..Default::default()
            },
            CanFrame::Error { classes, data } => Self {
                kind: v1::CanFrameKind::Error as i32,
                data: data.to_vec(),
                error_classes: classes
                    .iter()
                    .copied()
                    .map(error_class_to_proto)
                    .map(|value| value as i32)
                    .collect(),
                ..Default::default()
            },
        }
    }
}

impl TryFrom<v1::CanTimestamp> for CanTimestamp {
    type Error = HalError;

    fn try_from(value: v1::CanTimestamp) -> HalResult<Self> {
        let source =
            match required_enum::<v1::CanTimestampSource>(value.source, "can_timestamp.source")? {
                v1::CanTimestampSource::Hardware => CanTimestampSource::Hardware,
                v1::CanTimestampSource::Kernel => CanTimestampSource::Kernel,
                v1::CanTimestampSource::HostMonotonic => CanTimestampSource::HostMonotonic,
                v1::CanTimestampSource::Unspecified => {
                    return Err(invalid("can_timestamp.source", "is required"));
                }
            };
        CanTimestamp::new(value.timestamp_ns, source, value.clock_domain)
            .map_err(|_| invalid("can_timestamp.clock_domain", "is invalid"))
    }
}

impl From<&CanTimestamp> for v1::CanTimestamp {
    fn from(value: &CanTimestamp) -> Self {
        Self {
            timestamp_ns: value.timestamp_ns(),
            source: match value.source() {
                CanTimestampSource::Hardware => v1::CanTimestampSource::Hardware,
                CanTimestampSource::Kernel => v1::CanTimestampSource::Kernel,
                CanTimestampSource::HostMonotonic => v1::CanTimestampSource::HostMonotonic,
            } as i32,
            clock_domain: value.clock_domain().to_owned(),
        }
    }
}

impl TryFrom<v1::ReceivedCanFrame> for ReceivedCanFrame {
    type Error = HalError;

    fn try_from(value: v1::ReceivedCanFrame) -> HalResult<Self> {
        let frame = CanFrame::try_from(required(value.frame, "received_can_frame.frame")?)?;
        let timestamp = value.timestamp.map(CanTimestamp::try_from).transpose()?;
        Ok(Self::new(frame, timestamp))
    }
}

impl From<&ReceivedCanFrame> for v1::ReceivedCanFrame {
    fn from(value: &ReceivedCanFrame) -> Self {
        Self {
            frame: Some(value.frame().into()),
            timestamp: value.timestamp().map(Into::into),
        }
    }
}

impl TryFrom<v1::CanBitTiming> for CanBitTiming {
    type Error = HalError;

    fn try_from(value: v1::CanBitTiming) -> HalResult<Self> {
        let sample_point_permill = value
            .sample_point_permill
            .map(|value| {
                u16::try_from(value)
                    .map_err(|_| invalid("can_bit_timing.sample_point_permill", "is out of range"))
            })
            .transpose()?;
        let sjw = value
            .sjw
            .map(|value| {
                u16::try_from(value).map_err(|_| invalid("can_bit_timing.sjw", "is out of range"))
            })
            .transpose()?;
        CanBitTiming::new(value.bitrate, sample_point_permill, sjw)
            .map_err(|_| invalid("can_bit_timing", "contains invalid timing values"))
    }
}

impl From<&CanBitTiming> for v1::CanBitTiming {
    fn from(value: &CanBitTiming) -> Self {
        Self {
            bitrate: value.bitrate(),
            sample_point_permill: value.sample_point_permill().map(u32::from),
            sjw: value.sjw().map(u32::from),
        }
    }
}

impl TryFrom<v1::CanLinkExpectation> for CanLinkExpectation {
    type Error = HalError;

    fn try_from(value: v1::CanLinkExpectation) -> HalResult<Self> {
        let mode = value
            .mode
            .map(|value| mode_from_proto(value, "can_link_expectation.mode"))
            .transpose()?;
        CanLinkExpectation::new(
            mode,
            value.nominal_bitrate,
            value.data_bitrate,
            value.listen_only,
            value.loopback,
        )
        .map_err(|_| invalid("can_link_expectation", "contains inconsistent expectations"))
    }
}

impl From<&CanLinkExpectation> for v1::CanLinkExpectation {
    fn from(value: &CanLinkExpectation) -> Self {
        Self {
            mode: value.mode().map(mode_to_proto).map(|value| value as i32),
            nominal_bitrate: value.nominal_bitrate(),
            data_bitrate: value.data_bitrate(),
            listen_only: value.listen_only(),
            loopback: value.loopback(),
        }
    }
}

impl TryFrom<v1::CanConfigureConfig> for CanConfigureConfig {
    type Error = HalError;

    fn try_from(value: v1::CanConfigureConfig) -> HalResult<Self> {
        let mode = mode_from_proto(value.mode, "can_configure_config.mode")?;
        let nominal =
            CanBitTiming::try_from(required(value.nominal, "can_configure_config.nominal")?)?;
        let data = value.data.map(CanBitTiming::try_from).transpose()?;
        CanConfigureConfig::new_with_restart(
            mode,
            nominal,
            data,
            value.listen_only,
            value.loopback,
            value.restart_ms,
        )
        .map_err(|_| invalid("can_configure_config", "is inconsistent"))
    }
}

impl From<&CanConfigureConfig> for v1::CanConfigureConfig {
    fn from(value: &CanConfigureConfig) -> Self {
        Self {
            mode: mode_to_proto(value.mode()) as i32,
            nominal: Some(value.nominal().into()),
            data: value.data().map(Into::into),
            listen_only: value.listen_only(),
            loopback: value.loopback(),
            restart_ms: value.restart_ms(),
        }
    }
}

impl TryFrom<v1::CanOpenConfig> for CanOpenConfig {
    type Error = HalError;

    fn try_from(value: v1::CanOpenConfig) -> HalResult<Self> {
        match required(value.config, "can_open_config.config")? {
            v1::can_open_config::Config::Attach(value) => Ok(Self::Attach(value.try_into()?)),
            v1::can_open_config::Config::Configure(value) => Ok(Self::Configure(value.try_into()?)),
        }
    }
}

impl From<&CanOpenConfig> for v1::CanOpenConfig {
    fn from(value: &CanOpenConfig) -> Self {
        let config = match value {
            CanOpenConfig::Attach(value) => v1::can_open_config::Config::Attach(value.into()),
            CanOpenConfig::Configure(value) => v1::can_open_config::Config::Configure(value.into()),
        };
        Self {
            config: Some(config),
        }
    }
}

impl TryFrom<v1::CanActiveConfig> for CanActiveConfig {
    type Error = HalError;

    fn try_from(value: v1::CanActiveConfig) -> HalResult<Self> {
        let mode = mode_from_proto(value.mode, "can_active_config.mode")?;
        let nominal =
            CanBitTiming::try_from(required(value.nominal, "can_active_config.nominal")?)?;
        let data = value.data.map(CanBitTiming::try_from).transpose()?;
        CanActiveConfig::new(
            mode,
            nominal,
            data,
            value.listen_only,
            value.loopback,
            value.clock_domain,
        )
        .map_err(|_| {
            invalid(
                "can_active_config",
                "is inconsistent or has an invalid domain",
            )
        })
    }
}

impl From<&CanActiveConfig> for v1::CanActiveConfig {
    fn from(value: &CanActiveConfig) -> Self {
        Self {
            mode: mode_to_proto(value.mode()) as i32,
            nominal: Some(value.nominal().into()),
            data: value.data().map(Into::into),
            listen_only: value.listen_only(),
            loopback: value.loopback(),
            clock_domain: value.clock_domain().to_owned(),
        }
    }
}

impl From<CanFrameClasses> for v1::CanFrameClasses {
    fn from(value: CanFrameClasses) -> Self {
        Self {
            data: value.data(),
            remote: value.remote(),
            error: value.error(),
        }
    }
}

impl From<v1::CanFrameClasses> for CanFrameClasses {
    fn from(value: v1::CanFrameClasses) -> Self {
        Self::new(value.data, value.remote, value.error)
    }
}

impl TryFrom<v1::CanFilter> for CanFilter {
    type Error = HalError;

    fn try_from(value: v1::CanFilter) -> HalResult<Self> {
        let format = match required_enum::<v1::CanIdFormat>(value.format, "can_filter.format")? {
            v1::CanIdFormat::Standard => CanIdFormat::Standard,
            v1::CanIdFormat::Extended => CanIdFormat::Extended,
            v1::CanIdFormat::Either => CanIdFormat::Either,
            v1::CanIdFormat::Unspecified => {
                return Err(invalid("can_filter.format", "is required"));
            }
        };
        let classes = CanFrameClasses::from(required(value.classes, "can_filter.classes")?);
        CanFilter::new(value.id, value.mask, format, classes)
            .map_err(|_| invalid("can_filter", "contains invalid bounds or frame classes"))
    }
}

impl From<&CanFilter> for v1::CanFilter {
    fn from(value: &CanFilter) -> Self {
        Self {
            id: value.id(),
            mask: value.mask(),
            format: match value.format() {
                CanIdFormat::Standard => v1::CanIdFormat::Standard,
                CanIdFormat::Extended => v1::CanIdFormat::Extended,
                CanIdFormat::Either => v1::CanIdFormat::Either,
            } as i32,
            classes: Some(value.classes().into()),
        }
    }
}

impl TryFrom<v1::CanFilterSet> for CanFilterSet {
    type Error = HalError;

    fn try_from(value: v1::CanFilterSet) -> HalResult<Self> {
        if value.filters.len() > seeed_hal_can::MAX_CAN_FILTERS {
            return Err(invalid(
                "can_filter_set.filters",
                "exceeds the 64-filter bound",
            ));
        }
        let filters = value
            .filters
            .into_iter()
            .map(CanFilter::try_from)
            .collect::<HalResult<Vec<_>>>()?;
        CanFilterSet::new(filters).map_err(|_| invalid("can_filter_set.filters", "is invalid"))
    }
}

impl From<&CanFilterSet> for v1::CanFilterSet {
    fn from(value: &CanFilterSet) -> Self {
        Self {
            filters: value.as_slice().iter().map(Into::into).collect(),
        }
    }
}

impl TryFrom<v1::CanBusStatus> for CanBusStatus {
    type Error = HalError;

    fn try_from(value: v1::CanBusStatus) -> HalResult<Self> {
        let state = match required_enum::<v1::CanBusState>(value.state, "can_bus_status.state")? {
            v1::CanBusState::Active => CanBusState::Active,
            v1::CanBusState::Warning => CanBusState::Warning,
            v1::CanBusState::Passive => CanBusState::Passive,
            v1::CanBusState::BusOff => CanBusState::BusOff,
            v1::CanBusState::Stopped => CanBusState::Stopped,
            v1::CanBusState::Unknown => CanBusState::Unknown,
            v1::CanBusState::Unspecified => {
                return Err(invalid("can_bus_status.state", "is required"));
            }
        };
        Ok(Self::new(
            state,
            value.tx_error_counter,
            value.rx_error_counter,
        ))
    }
}

impl From<&CanBusStatus> for v1::CanBusStatus {
    fn from(value: &CanBusStatus) -> Self {
        Self {
            state: match value.state() {
                CanBusState::Active => v1::CanBusState::Active,
                CanBusState::Warning => v1::CanBusState::Warning,
                CanBusState::Passive => v1::CanBusState::Passive,
                CanBusState::BusOff => v1::CanBusState::BusOff,
                CanBusState::Stopped => v1::CanBusState::Stopped,
                CanBusState::Unknown => v1::CanBusState::Unknown,
            } as i32,
            tx_error_counter: value.tx_error_counter(),
            rx_error_counter: value.rx_error_counter(),
        }
    }
}

pub fn enumerate_can_response_from_proto(
    value: v1::EnumerateCanResponse,
) -> HalResult<Vec<ResourceDescriptor>> {
    value
        .resources
        .into_iter()
        .map(|value| {
            let descriptor = ResourceDescriptor::try_from(value)?;
            if descriptor.transport() != seeed_hal_core::TransportKind::Can {
                return Err(invalid("enumerate_can.resources.transport", "must be CAN"));
            }
            Ok(descriptor)
        })
        .collect()
}

pub fn open_can_request_from_proto(
    value: v1::OpenCanRequest,
) -> HalResult<(ResourceSelector, LeaseMode, CanOpenConfig, CanFilterSet)> {
    let selector = ResourceSelector::try_from(required(value.selector, "open_can.selector")?)?;
    if selector.transport() != seeed_hal_core::TransportKind::Can {
        return Err(invalid("open_can.selector.transport", "must be CAN"));
    }
    let mode = lease_mode_from_proto(value.mode, "open_can.mode")?;
    let config = CanOpenConfig::try_from(required(value.config, "open_can.config")?)?;
    let filters = CanFilterSet::try_from(required(value.filters, "open_can.filters")?)?;
    Ok((selector, mode, config, filters))
}

pub fn open_can_response_from_proto(
    value: v1::OpenCanResponse,
    expected_mode: LeaseMode,
) -> HalResult<(SessionId, LeaseToken)> {
    let (session, lease) = parse_session_lease(value.session_id, value.lease)?;
    if lease.mode() != expected_mode {
        return Err(invalid(
            "open_can_response.lease.mode",
            "does not match the requested mode",
        ));
    }
    Ok((session, lease))
}

pub fn send_can_frames_from_proto(value: Vec<v1::CanFrame>) -> HalResult<Vec<CanFrame>> {
    if !(1..=MAX_CAN_BATCH_FRAMES).contains(&value.len()) {
        return Err(invalid("can_send.frames", "must contain 1..=64 frames"));
    }
    value
        .into_iter()
        .map(CanFrame::try_from)
        .collect::<HalResult<Vec<_>>>()
}

pub fn can_send_request_from_proto(
    value: v1::CanSendRequest,
) -> HalResult<(SessionId, LeaseToken, Vec<CanFrame>)> {
    let (session, lease) = parse_session_lease(value.session_id, value.lease)?;
    let frames = send_can_frames_from_proto(value.frames)?;
    Ok((session, lease, frames))
}

fn validate_input_count(input_count: usize) -> HalResult<()> {
    if !(1..=MAX_CAN_BATCH_FRAMES).contains(&input_count) {
        return Err(invalid("can_send.input_count", "must be 1..=64"));
    }
    Ok(())
}

pub fn can_send_response_from_proto(
    value: v1::CanSendResponse,
    input_count: usize,
) -> HalResult<Result<(), CanBatchSendError>> {
    validate_input_count(input_count)?;
    let committed = usize::try_from(value.committed_count)
        .map_err(|_| invalid("can_send_response.committed_count", "is out of range"))?;
    match value.error {
        None if committed == input_count => Ok(Ok(())),
        None => Err(invalid(
            "can_send_response.committed_count",
            "must equal the input count when error is absent",
        )),
        Some(error) if committed < input_count => Ok(Err(CanBatchSendError::backend_prefix(
            error_from_proto(error)?,
            committed,
        ))),
        Some(_) => Err(invalid(
            "can_send_response.committed_count",
            "must be a strict input prefix when error is present",
        )),
    }
}

pub fn can_send_response_to_proto(
    result: Result<(), &CanBatchSendError>,
    input_count: usize,
) -> HalResult<v1::CanSendResponse> {
    validate_input_count(input_count)?;
    match result {
        Ok(()) => Ok(v1::CanSendResponse {
            committed_count: u32::try_from(input_count)
                .expect("validated CAN batch count fits u32"),
            error: None,
        }),
        Err(error) if error.committed() < input_count => Ok(v1::CanSendResponse {
            committed_count: u32::try_from(error.committed())
                .expect("validated CAN committed prefix fits u32"),
            error: Some(error.error().into()),
        }),
        Err(_) => Err(invalid(
            "can_send_response.committed_count",
            "must be a strict input prefix when error is present",
        )),
    }
}

pub fn can_receive_parameters(max_frames: u32, timeout_ms: u64) -> HalResult<(usize, Duration)> {
    let max_frames = usize::try_from(max_frames)
        .map_err(|_| invalid("can_receive.max_frames", "is out of range"))?;
    if !(1..=MAX_CAN_BATCH_FRAMES).contains(&max_frames) {
        return Err(invalid("can_receive.max_frames", "must be 1..=64"));
    }
    Ok((max_frames, Duration::from_millis(timeout_ms)))
}

pub fn can_receive_request_from_proto(
    value: v1::CanReceiveRequest,
) -> HalResult<(SessionId, LeaseToken, usize, Duration)> {
    let (session, lease) = parse_session_lease(value.session_id, value.lease)?;
    let (max_frames, timeout) = can_receive_parameters(value.max_frames, value.timeout_ms)?;
    Ok((session, lease, max_frames, timeout))
}

pub fn received_can_frames_from_proto(
    value: Vec<v1::ReceivedCanFrame>,
    requested_max: usize,
) -> HalResult<Vec<ReceivedCanFrame>> {
    if !(1..=MAX_CAN_BATCH_FRAMES).contains(&requested_max) {
        return Err(invalid("can_receive.requested_max", "must be 1..=64"));
    }
    if value.len() > requested_max {
        return Err(invalid(
            "can_receive_response.frames",
            "exceeds the requested maximum",
        ));
    }
    value
        .into_iter()
        .map(ReceivedCanFrame::try_from)
        .collect::<HalResult<Vec<_>>>()
}

pub fn can_receive_response_from_proto(
    value: v1::CanReceiveResponse,
    requested_max: usize,
) -> HalResult<Vec<ReceivedCanFrame>> {
    received_can_frames_from_proto(value.frames, requested_max)
}

pub fn replace_can_filters_request_from_proto(
    value: v1::ReplaceCanFiltersRequest,
) -> HalResult<(SessionId, LeaseToken, CanFilterSet)> {
    let (session, lease) = parse_session_lease(value.session_id, value.lease)?;
    let filters = CanFilterSet::try_from(required(value.filters, "replace_can_filters.filters")?)?;
    Ok((session, lease, filters))
}

pub fn get_can_bus_status_request_from_proto(
    value: v1::GetCanBusStatusRequest,
) -> HalResult<(SessionId, LeaseToken)> {
    parse_session_lease(value.session_id, value.lease)
}

pub fn get_can_bus_status_response_from_proto(
    value: v1::GetCanBusStatusResponse,
) -> HalResult<CanBusStatus> {
    CanBusStatus::try_from(required(value.status, "get_can_bus_status.status")?)
}
