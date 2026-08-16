#![cfg(target_os = "linux")]

use seeed_hal_can::{
    CanActiveConfig, CanBitTiming, CanBusState, CanBusStatus, CanConfigureConfig,
    CanLinkExpectation, CanMode,
};
use seeed_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor};
use socketcan::nl::{
    CanCtrlMode, CanCtrlModes, CanInterface, CanState, InterfaceCanParams, InterfaceDetails, Mtu,
};

const CLOCK_DOMAIN: &str = "host-monotonic";
const DEFAULT_BITRATE: u32 = 500_000;

#[derive(Debug)]
pub(crate) struct LinkLease {
    interface: CanInterface,
    snapshot: Option<LinkSnapshot>,
    applied: Option<LinkFingerprint>,
    pub(crate) active: CanActiveConfig,
}

#[derive(Clone, Debug)]
struct LinkSnapshot {
    details: InterfaceDetails,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LinkFingerprint {
    is_up: bool,
    mtu: Option<Mtu>,
    nominal: Option<TimingFingerprint>,
    data: Option<TimingFingerprint>,
    restart_ms: Option<u32>,
    mode: CanMode,
    listen_only: bool,
    loopback: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TimingFingerprint {
    bitrate: u32,
    sample_point: u32,
    sjw: u32,
}

impl LinkLease {
    pub(crate) fn attach(
        interface: &str,
        expectation: &CanLinkExpectation,
        descriptor: &ResourceDescriptor,
    ) -> HalResult<Self> {
        let interface_handle = CanInterface::open(interface)
            .map_err(|error| map_link_error("can.open", error, descriptor))?;
        let details = interface_handle
            .details()
            .map_err(|error| map_nl_error("can.open", error, descriptor))?;
        let active = active_config(&details, descriptor)?;
        verify_expectation(expectation, &active, descriptor)?;
        Ok(Self {
            interface: interface_handle,
            snapshot: None,
            applied: None,
            active,
        })
    }

    pub(crate) fn configure(
        interface: &str,
        request: &CanConfigureConfig,
        descriptor: &ResourceDescriptor,
    ) -> HalResult<Self> {
        let interface_handle = CanInterface::open(interface)
            .map_err(|error| map_link_error("can.configure", error, descriptor))?;
        let before = interface_handle
            .details()
            .map_err(|error| map_nl_error("can.configure", error, descriptor))?;
        let snapshot = LinkSnapshot {
            details: before.clone(),
        };

        let result = configure_link(&interface_handle, &before, request, descriptor);
        if let Err(error) = result {
            let _ = restore_snapshot(&interface_handle, &snapshot.details);
            return Err(error);
        }

        let after = interface_handle
            .details()
            .map_err(|error| map_nl_error("can.configure", error, descriptor))?;
        let active = active_config(&after, descriptor)?;
        verify_config(request, &active, descriptor)?;
        let applied = fingerprint(&after, &active);
        Ok(Self {
            interface: interface_handle,
            snapshot: Some(snapshot),
            applied: Some(applied),
            active,
        })
    }

    pub(crate) fn bus_status(
        &self,
        descriptor: &ResourceDescriptor,
    ) -> HalResult<CanBusStatus> {
        let details = self
            .interface
            .details()
            .map_err(|error| map_nl_error("can.status", error, descriptor))?;
        let state = details
            .can
            .state
            .map(map_bus_state)
            .unwrap_or(CanBusState::Unknown);
        let (tx, rx) = details
            .can
            .berr_counter
            .map(|counter| (Some(u32::from(counter.txerr)), Some(u32::from(counter.rxerr))))
            .unwrap_or((None, None));
        Ok(CanBusStatus::new(state, tx, rx))
    }

    pub(crate) fn close(&mut self, descriptor: &ResourceDescriptor) -> HalResult<()> {
        let Some(snapshot) = self.snapshot.take() else {
            return Ok(());
        };

        let current = self
            .interface
            .details()
            .map_err(|error| map_nl_error("can.close", error, descriptor))?;
        let current_active = active_config(&current, descriptor)?;
        if self
            .applied
            .as_ref()
            .is_some_and(|applied| fingerprint(&current, &current_active) != *applied)
        {
            return Err(HalError::new(
                "can.configuration.conflict",
                ErrorCategory::Conflict,
                "can.close",
                false,
                "SocketCAN link changed externally; refusing to overwrite current state",
            )?
            .with_resource_id(descriptor.id().clone()));
        }
        restore_snapshot(&self.interface, &snapshot.details)
            .map_err(|error| error.with_resource_id(descriptor.id().clone()))
    }
}

pub(crate) fn details_for_descriptor(
    interface: &str,
) -> Result<InterfaceDetails, Box<dyn std::error::Error + Send + Sync>> {
    let interface = CanInterface::open(interface)?;
    Ok(interface.details()?)
}

pub(crate) fn capabilities_for_details(
    details: &InterfaceDetails,
    virtual_interface: bool,
) -> (bool, bool) {
    let fd = details.mtu == Some(Mtu::Fd);
    let configure = !virtual_interface;
    (fd, configure)
}

fn configure_link(
    interface: &CanInterface,
    before: &InterfaceDetails,
    request: &CanConfigureConfig,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    if before.is_up {
        interface
            .bring_down()
            .map_err(|error| map_nl_error("can.configure", error, descriptor))?;
    }

    let mut modes = CanCtrlModes::default();
    modes.add(CanCtrlMode::Fd, request.mode() == CanMode::Fd);
    modes.add(CanCtrlMode::ListenOnly, request.listen_only());
    modes.add(CanCtrlMode::Loopback, request.loopback());
    let params = InterfaceCanParams {
        bit_timing: Some(to_socketcan_timing(request.nominal())),
        data_bit_timing: request.data().map(to_socketcan_timing),
        ctrl_mode: Some(modes),
        restart_ms: Some(request.restart_ms().unwrap_or(0)),
        ..InterfaceCanParams::default()
    };
    interface
        .set_can_params(&params)
        .map_err(|error| map_nl_error("can.configure", error, descriptor))?;
    interface
        .set_mtu(if request.mode() == CanMode::Fd {
            Mtu::Fd
        } else {
            Mtu::Standard
        })
        .map_err(|error| map_nl_error("can.configure", error, descriptor))?;
    if before.is_up {
        interface
            .bring_up()
            .map_err(|error| map_nl_error("can.configure", error, descriptor))?;
    }
    Ok(())
}

fn restore_snapshot(
    interface: &CanInterface,
    details: &InterfaceDetails,
) -> HalResult<()> {
    if details.is_up {
        interface
            .bring_down()
            .map_err(|error| generic_link_error("can.close", error))?;
    }
    if let Some(mtu) = details.mtu {
        interface
            .set_mtu(mtu)
            .map_err(|error| generic_link_error("can.close", error))?;
    }
    if details.can.bit_timing.is_some()
        || details.can.data_bit_timing.is_some()
        || details.can.ctrl_mode.is_some()
        || details.can.restart_ms.is_some()
    {
        interface
            .set_can_params(&details.can)
            .map_err(|error| generic_link_error("can.close", error))?;
    }
    if details.is_up {
        interface
            .bring_up()
            .map_err(|error| generic_link_error("can.close", error))?;
    }
    Ok(())
}

fn active_config(details: &InterfaceDetails, descriptor: &ResourceDescriptor) -> HalResult<CanActiveConfig> {
    let mode = if details.mtu == Some(Mtu::Fd) {
        CanMode::Fd
    } else {
        CanMode::Classic
    };
    let nominal = details
        .can
        .bit_timing
        .map(from_socketcan_timing)
        .transpose()?
        .unwrap_or(CanBitTiming::new(DEFAULT_BITRATE, None, None)?);
    let data = if mode == CanMode::Fd {
        Some(
            details
                .can
                .data_bit_timing
                .map(from_socketcan_timing)
                .transpose()?
                .unwrap_or(nominal),
        )
    } else {
        None
    };
    let (listen_only, loopback) = details
        .can
        .ctrl_mode
        .map(|modes| {
            (
                modes.has_mode(CanCtrlMode::ListenOnly),
                modes.has_mode(CanCtrlMode::Loopback),
            )
        })
        .unwrap_or((false, false));
    CanActiveConfig::new(mode, nominal, data, listen_only, loopback, CLOCK_DOMAIN)
        .map_err(|error| error.with_resource_id(descriptor.id().clone()))
}

fn verify_expectation(
    expectation: &CanLinkExpectation,
    active: &CanActiveConfig,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    let mismatch = expectation.mode().is_some_and(|mode| mode != active.mode())
        || expectation
            .nominal_bitrate()
            .is_some_and(|bitrate| bitrate != active.nominal().bitrate())
        || expectation.data_bitrate().is_some_and(|bitrate| {
            active.data().is_none_or(|timing| timing.bitrate() != bitrate)
        })
        || expectation
            .listen_only()
            .is_some_and(|value| value != active.listen_only())
        || expectation
            .loopback()
            .is_some_and(|value| value != active.loopback());
    if mismatch {
        return Err(HalError::new(
            "can.configuration.mismatch",
            ErrorCategory::Conflict,
            "can.open",
            false,
            "Attach expectations do not match active SocketCAN link state",
        )?
        .with_resource_id(descriptor.id().clone()));
    }
    Ok(())
}

fn verify_config(
    request: &CanConfigureConfig,
    active: &CanActiveConfig,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    let mismatch = request.mode() != active.mode()
        || request.nominal().bitrate() != active.nominal().bitrate()
        || request
            .data()
            .is_some_and(|timing| active.data().is_none_or(|actual| timing.bitrate() != actual.bitrate()))
        || request.listen_only() != active.listen_only()
        || request.loopback() != active.loopback();
    if mismatch {
        return Err(HalError::new(
            "can.configuration.mismatch",
            ErrorCategory::Conflict,
            "can.configure",
            false,
            "SocketCAN link did not expose the requested configuration after netlink apply",
        )?
        .with_resource_id(descriptor.id().clone()));
    }
    Ok(())
}

fn fingerprint(details: &InterfaceDetails, active: &CanActiveConfig) -> LinkFingerprint {
    LinkFingerprint {
        is_up: details.is_up,
        mtu: details.mtu,
        nominal: details.can.bit_timing.map(timing_fingerprint),
        data: details.can.data_bit_timing.map(timing_fingerprint),
        restart_ms: details.can.restart_ms,
        mode: active.mode(),
        listen_only: active.listen_only(),
        loopback: active.loopback(),
    }
}

fn timing_fingerprint(timing: socketcan::nl::CanBitTiming) -> TimingFingerprint {
    TimingFingerprint {
        bitrate: timing.bitrate,
        sample_point: timing.sample_point,
        sjw: timing.sjw,
    }
}

fn to_socketcan_timing(timing: &CanBitTiming) -> socketcan::nl::CanBitTiming {
    socketcan::nl::CanBitTiming {
        bitrate: timing.bitrate(),
        sample_point: u32::from(timing.sample_point_permill().unwrap_or(0)),
        sjw: u32::from(timing.sjw().unwrap_or(0)),
        ..socketcan::nl::CanBitTiming::default()
    }
}

fn from_socketcan_timing(timing: socketcan::nl::CanBitTiming) -> HalResult<CanBitTiming> {
    let sample_point = u16::try_from(timing.sample_point).ok().filter(|value| *value != 0);
    let sjw = u16::try_from(timing.sjw).ok().filter(|value| *value != 0);
    CanBitTiming::new(
        timing.bitrate,
        sample_point,
        sjw,
    )
}

fn map_bus_state(state: CanState) -> CanBusState {
    match state {
        CanState::ErrorActive => CanBusState::Active,
        CanState::ErrorWarning => CanBusState::Warning,
        CanState::ErrorPassive => CanBusState::Passive,
        CanState::BusOff => CanBusState::BusOff,
        CanState::Stopped | CanState::Sleeping => CanBusState::Stopped,
    }
}

fn map_link_error(
    operation: &'static str,
    error: impl std::error::Error,
    descriptor: &ResourceDescriptor,
) -> HalError {
    generic_link_error(operation, error).with_resource_id(descriptor.id().clone())
}

fn map_nl_error<T: std::fmt::Debug, P: std::fmt::Debug>(
    operation: &'static str,
    error: neli::err::NlError<T, P>,
    descriptor: &ResourceDescriptor,
) -> HalError {
    match error {
        neli::err::NlError::Nlmsgerr(message) => {
            let raw_code = message.error.saturating_neg();
            os_link_error(operation, raw_code, message.to_string())
                .with_resource_id(descriptor.id().clone())
        }
        neli::err::NlError::Wrapped(neli::err::WrappedError::IOError(error)) => {
            let raw_code = error.raw_os_error();
            let mapped = raw_code.map_or_else(
                || generic_link_error(operation, error),
                |raw_code| os_link_error(operation, raw_code, error.to_string()),
            );
            mapped.with_resource_id(descriptor.id().clone())
        }
        error => generic_link_error(operation, error).with_resource_id(descriptor.id().clone()),
    }
}

fn os_link_error(operation: &'static str, raw_code: i32, message: String) -> HalError {
    let (name, category, retryable) = match raw_code {
        libc::EPERM | libc::EACCES => (
            "runtime.transport.permission_denied",
            ErrorCategory::Conflict,
            false,
        ),
        libc::EINVAL | libc::EOPNOTSUPP | libc::ERANGE => (
            "runtime.transport.unsupported_configuration",
            ErrorCategory::InvalidArgument,
            false,
        ),
        libc::ENODEV | libc::ENOENT => {
            ("runtime.resource.not_found", ErrorCategory::NotFound, false)
        }
        libc::EBUSY => ("runtime.transport.busy", ErrorCategory::Conflict, true),
        _ => (
            "runtime.transport.unavailable",
            ErrorCategory::Unavailable,
            true,
        ),
    };
    HalError::new(
        name,
        category,
        operation,
        retryable,
        format!("SocketCAN netlink failed raw_os_error={raw_code}: {message}"),
    )
    .expect("static SocketCAN error metadata is valid")
    .with_platform_code(raw_code.to_string())
    .expect("decimal OS error code is a valid platform code")
}

fn generic_link_error(operation: &'static str, error: impl std::error::Error) -> HalError {
    HalError::new(
        "runtime.transport.unavailable",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!("SocketCAN netlink operation failed: {error}"),
    )
    .expect("static SocketCAN error metadata is valid")
}
