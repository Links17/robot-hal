#![cfg(target_os = "linux")]

use seeed_hal_can::{
    CanActiveConfig, CanBitTiming, CanBusState, CanBusStatus, CanConfigureConfig,
    CanLinkExpectation, CanMode,
};
use seeed_hal_core::{
    ErrorCategory, ErrorContext, HalError, HalResult, ResourceDescriptor, ResourceId,
};
use socketcan::nl::{
    CanCtrlMode, CanCtrlModes, CanInterface, CanState, InterfaceCanParams, InterfaceDetails, Mtu,
};

use crate::identity::{identity_from_metadata, metadata_from_sysfs};

const CLOCK_DOMAIN: &str = "host-monotonic";

#[derive(Debug)]
pub(crate) struct LinkLease {
    interface: CanInterface,
    descriptor: ResourceDescriptor,
    snapshot: Option<LinkSnapshot>,
    applied: Option<LinkFingerprint>,
    pub(crate) active: CanActiveConfig,
}

#[derive(Clone, Debug)]
struct LinkSnapshot {
    details: InterfaceDetails,
    fingerprint: LinkFingerprint,
    restore_evidence: RestoreEvidence,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LinkFingerprint {
    interface_index: u32,
    interface_name: String,
    physical_identity: ResourceId,
    is_up: bool,
    mtu: Option<Mtu>,
    nominal: Option<TimingFingerprint>,
    data: Option<TimingFingerprint>,
    restart_ms: Option<u32>,
    control_modes: [bool; 9],
    termination: Option<u16>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TimingFingerprint {
    bitrate: u32,
    sample_point: u32,
    tq: u32,
    prop_seg: u32,
    phase_seg1: u32,
    phase_seg2: u32,
    sjw: u32,
    brp: u32,
}

struct ConfigureFailure {
    error: HalError,
    rollback_required: bool,
    restore_evidence: RestoreEvidence,
}

#[derive(Clone, Copy, Debug, Default)]
struct RestoreEvidence {
    control_modes: [bool; 3],
}

impl LinkLease {
    pub(crate) fn attach(
        interface: &str,
        expectation: &CanLinkExpectation,
        descriptor: &ResourceDescriptor,
    ) -> HalResult<Self> {
        let interface_handle = CanInterface::open(interface)
            .map_err(|error| map_link_io_error("can.open", error.into(), descriptor))?;
        let details = interface_handle
            .details()
            .map_err(|error| map_nl_error("can.open", error, descriptor))?;
        let current = fingerprint(&details, "can.open", descriptor)?;
        verify_physical_identity(&current, descriptor, "can.open")?;
        let active = active_config(&details, descriptor, "can.open")?;
        verify_expectation(expectation, &active, descriptor)?;
        Ok(Self {
            interface: interface_handle,
            descriptor: descriptor.clone(),
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
            .map_err(|error| map_link_io_error("can.configure", error.into(), descriptor))?;
        let before = interface_handle
            .details()
            .map_err(|error| map_nl_error("can.configure", error, descriptor))?;
        let snapshot_fingerprint = fingerprint(&before, "can.configure", descriptor)?;
        verify_physical_identity(&snapshot_fingerprint, descriptor, "can.configure")?;
        let mut snapshot = LinkSnapshot {
            details: before.clone(),
            fingerprint: snapshot_fingerprint,
            restore_evidence: RestoreEvidence::default(),
        };

        match configure_link(&interface_handle, &before, request, descriptor) {
            Ok(restore_evidence) => snapshot.restore_evidence = restore_evidence,
            Err(failure) => {
                snapshot.restore_evidence = failure.restore_evidence;
                if failure.rollback_required {
                    return Err(rollback_after_failure(
                        &interface_handle,
                        &snapshot,
                        descriptor,
                        failure.error,
                    ));
                }
                return Err(failure.error);
            }
        }

        let after = match interface_handle.details() {
            Ok(details) => details,
            Err(error) => {
                let error = map_nl_error("can.configure", error, descriptor);
                return Err(rollback_after_failure(
                    &interface_handle,
                    &snapshot,
                    descriptor,
                    error,
                ));
            }
        };
        let active = match active_config(&after, descriptor, "can.configure") {
            Ok(active) => active,
            Err(error) => {
                return Err(rollback_after_failure(
                    &interface_handle,
                    &snapshot,
                    descriptor,
                    error,
                ));
            }
        };
        if let Err(error) = verify_config(request, &after, &active, descriptor) {
            return Err(rollback_after_failure(
                &interface_handle,
                &snapshot,
                descriptor,
                error,
            ));
        }
        let applied = match fingerprint(&after, "can.configure", descriptor) {
            Ok(applied) => applied,
            Err(error) => {
                return Err(rollback_after_failure(
                    &interface_handle,
                    &snapshot,
                    descriptor,
                    error,
                ));
            }
        };
        if let Err(error) = verify_physical_identity(&applied, descriptor, "can.configure") {
            return Err(rollback_after_failure(
                &interface_handle,
                &snapshot,
                descriptor,
                error,
            ));
        }
        Ok(Self {
            interface: interface_handle,
            descriptor: descriptor.clone(),
            snapshot: Some(snapshot),
            applied: Some(applied),
            active,
        })
    }

    pub(crate) fn bus_status(
        &self,
        descriptor: &ResourceDescriptor,
    ) -> HalResult<CanBusStatus> {
        bus_status(&self.interface, descriptor)
    }

    pub(crate) fn close(&mut self, descriptor: &ResourceDescriptor) -> HalResult<()> {
        let Some(snapshot) = self.snapshot.clone()
        else {
            return Ok(());
        };

        let current = self
            .interface
            .details()
            .map_err(|error| map_nl_error("can.close", error, descriptor))?;
        let current = fingerprint(&current, "can.close", descriptor)?;
        if self
            .applied
            .as_ref()
            .is_some_and(|applied| current != *applied)
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
        restore_and_verify(&self.interface, &snapshot, descriptor)?;
        self.snapshot = None;
        self.applied = None;
        Ok(())
    }

    pub(crate) fn rollback_after_open_failure(
        &mut self,
        descriptor: &ResourceDescriptor,
        primary: HalError,
    ) -> HalError {
        match self.close(descriptor) {
            Ok(()) => primary,
            Err(rollback) => rollback_failure(primary, rollback, descriptor),
        }
    }
}

impl Drop for LinkLease {
    fn drop(&mut self) {
        if self.snapshot.is_some() {
            let descriptor = self.descriptor.clone();
            let _ = self.close(&descriptor);
        }
    }
}

pub(crate) fn details_for_descriptor(
    interface: &str,
) -> Result<InterfaceDetails, Box<dyn std::error::Error + Send + Sync>> {
    let interface = CanInterface::open(interface)?;
    Ok(interface.details()?)
}

pub(crate) fn bus_status_for_interface(
    interface: &str,
    descriptor: &ResourceDescriptor,
) -> HalResult<CanBusStatus> {
    let interface = CanInterface::open(interface)
        .map_err(|error| map_link_io_error("can.status", error.into(), descriptor))?;
    bus_status(&interface, descriptor)
}

fn bus_status(
    interface: &CanInterface,
    descriptor: &ResourceDescriptor,
) -> HalResult<CanBusStatus> {
    let details = interface
        .details()
        .map_err(|error| map_nl_error("can.status", error, descriptor))?;
    let current = fingerprint(&details, "can.status", descriptor)?;
    verify_physical_identity(&current, descriptor, "can.status")?;
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

pub(crate) fn capabilities_for_details(
    details: &InterfaceDetails,
    nonvirtual_sysfs_evidence: bool,
) -> (bool, bool) {
    let fd = is_fd_active(details) || details.can.data_bit_timing_const.is_some();
    let configure = nonvirtual_sysfs_evidence && details.can.bit_timing_const.is_some();
    (fd, configure)
}

pub(crate) fn is_fd_active(details: &InterfaceDetails) -> bool {
    details.mtu == Some(Mtu::Fd)
        && details
            .can
            .ctrl_mode
            .is_some_and(|modes| modes.has_mode(CanCtrlMode::Fd))
}

fn configure_link(
    interface: &CanInterface,
    before: &InterfaceDetails,
    request: &CanConfigureConfig,
    descriptor: &ResourceDescriptor,
) -> Result<RestoreEvidence, ConfigureFailure> {
    let mut rollback_required = false;
    let mut restore_evidence = RestoreEvidence::default();
    let pending_restore_evidence = restore_evidence_for_configure(before, request);
    if before.is_up {
        if let Err(error) = interface.bring_down() {
            return Err(ConfigureFailure {
                error: map_nl_error("can.configure", error, descriptor),
                rollback_required,
                restore_evidence,
            });
        }
        rollback_required = true;
    }

    let mut modes = CanCtrlModes::default();
    for (mode, enabled) in configured_control_modes()
        .into_iter()
        .zip(requested_control_modes(request))
    {
        modes.add(mode, enabled);
    }
    let params = InterfaceCanParams {
        bit_timing: Some(to_socketcan_timing(request.nominal())),
        data_bit_timing: request.data().map(to_socketcan_timing),
        ctrl_mode: Some(modes),
        restart_ms: Some(request.restart_ms().unwrap_or(0)),
        ..InterfaceCanParams::default()
    };
    if let Err(error) = interface.set_can_params(&params) {
        return Err(ConfigureFailure {
            error: map_nl_error("can.configure", error, descriptor),
            rollback_required,
            restore_evidence,
        });
    }
    restore_evidence = pending_restore_evidence;
    rollback_required = true;
    let mtu = if request.mode() == CanMode::Fd {
        Mtu::Fd
    } else {
        Mtu::Standard
    };
    if let Err(error) = interface.set_mtu(mtu) {
        return Err(ConfigureFailure {
            error: map_nl_error("can.configure", error, descriptor),
            rollback_required,
            restore_evidence,
        });
    }
    if before.is_up {
        if let Err(error) = interface.bring_up() {
            return Err(ConfigureFailure {
                error: map_nl_error("can.configure", error, descriptor),
                rollback_required,
                restore_evidence,
            });
        }
    }
    Ok(restore_evidence)
}

fn restore_snapshot(
    interface: &CanInterface,
    snapshot: &LinkSnapshot,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    let details = &snapshot.details;
    interface
        .bring_down()
        .map_err(|error| map_nl_error("can.close", error, descriptor))?;
    if let Some(mtu) = details.mtu {
        interface
            .set_mtu(mtu)
            .map_err(|error| map_nl_error("can.close", error, descriptor))?;
    }

    let params = restore_params(details, snapshot.restore_evidence);
    interface
        .set_can_params(&params)
        .map_err(|error| map_nl_error("can.close", error, descriptor))?;
    if details.is_up {
        interface
            .bring_up()
            .map_err(|error| map_nl_error("can.close", error, descriptor))?;
    }
    Ok(())
}

fn restore_params(
    details: &InterfaceDetails,
    restore_evidence: RestoreEvidence,
) -> InterfaceCanParams {
    let ctrl_mode = restore_evidence
        .control_modes
        .iter()
        .any(|restore| *restore)
        .then(|| {
            let snapshot_modes = details.can.ctrl_mode.unwrap_or_default();
            let mut modes = CanCtrlModes::default();
            for (mode, restore) in configured_control_modes()
                .into_iter()
                .zip(restore_evidence.control_modes)
            {
                if restore {
                    modes.add(mode, snapshot_modes.has_mode(mode));
                }
            }
            modes
        });
    InterfaceCanParams {
        bit_timing: details.can.bit_timing,
        data_bit_timing: details.can.data_bit_timing,
        ctrl_mode,
        restart_ms: details.can.restart_ms,
        termination: details.can.termination,
        ..InterfaceCanParams::default()
    }
}

fn restore_and_verify(
    interface: &CanInterface,
    snapshot: &LinkSnapshot,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    restore_snapshot(interface, snapshot, descriptor)?;
    let restored = interface
        .details()
        .map_err(|error| map_nl_error("can.close", error, descriptor))?;
    let restored = fingerprint(&restored, "can.close", descriptor)?;
    if restored != snapshot.fingerprint {
        return Err(HalError::new(
            "can.configuration.rollback_failed",
            ErrorCategory::Internal,
            "can.close",
            true,
            "SocketCAN link did not match its snapshot after restore",
        )?
        .with_resource_id(descriptor.id().clone()));
    }
    Ok(())
}

fn rollback_after_failure(
    interface: &CanInterface,
    snapshot: &LinkSnapshot,
    descriptor: &ResourceDescriptor,
    primary: HalError,
) -> HalError {
    match restore_and_verify(interface, snapshot, descriptor) {
        Ok(()) => primary,
        Err(rollback) => rollback_failure(primary, rollback, descriptor),
    }
}

fn rollback_failure(
    primary: HalError,
    rollback: HalError,
    descriptor: &ResourceDescriptor,
) -> HalError {
    let context = ErrorContext::new([
        ("primary_error", primary.name().as_str().to_owned()),
        ("rollback_error", rollback.name().as_str().to_owned()),
    ])
    .expect("static rollback context keys are valid");
    HalError::new(
        "can.configuration.rollback_failed",
        ErrorCategory::Internal,
        primary.operation().as_str(),
        true,
        format!(
            "SocketCAN rollback failed after {}: {}",
            primary.name().as_str(),
            rollback.name().as_str()
        ),
    )
    .expect("static rollback error metadata is valid")
    .with_context(context)
    .with_resource_id(descriptor.id().clone())
}

fn active_config(
    details: &InterfaceDetails,
    descriptor: &ResourceDescriptor,
    operation: &'static str,
) -> HalResult<CanActiveConfig> {
    let result = (|| {
        let fd_mtu = details.mtu == Some(Mtu::Fd);
        let fd_control_mode = details
            .can
            .ctrl_mode
            .is_some_and(|modes| modes.has_mode(CanCtrlMode::Fd));
        let mode = match (fd_mtu, fd_control_mode) {
            (true, true) => CanMode::Fd,
            (false, false) => CanMode::Classic,
            _ => {
                return Err(HalError::new(
                    "runtime.transport.unavailable",
                    ErrorCategory::Unavailable,
                    operation,
                    true,
                    "SocketCAN MTU and FD control mode disagree",
                )
                .expect("static SocketCAN error metadata is valid"));
            }
        };
        let nominal = details
            .can
            .bit_timing
            .map(|timing| from_socketcan_timing(timing, operation, "nominal"))
            .transpose()?
            .ok_or_else(|| {
                HalError::new(
                    "runtime.transport.unsupported_configuration",
                    ErrorCategory::InvalidArgument,
                    operation,
                    false,
                    "SocketCAN did not report nominal bit timing; no active timing is fabricated",
                )
                .expect("static timing error metadata is valid")
            })?;
        let data = if mode == CanMode::Fd {
            Some(
                details
                    .can
                    .data_bit_timing
                    .map(|timing| from_socketcan_timing(timing, operation, "data"))
                    .transpose()?
                    .ok_or_else(|| {
                        HalError::new(
                            "runtime.transport.unsupported_configuration",
                            ErrorCategory::InvalidArgument,
                            operation,
                            false,
                            "SocketCAN FD link did not report data bit timing",
                        )
                        .expect("static timing error metadata is valid")
                    })?,
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
    })();
    result.map_err(|error| error.with_resource_id(descriptor.id().clone()))
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
    details: &InterfaceDetails,
    active: &CanActiveConfig,
    descriptor: &ResourceDescriptor,
) -> HalResult<()> {
    if !config_matches(request, details, active) {
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

fn config_matches(
    request: &CanConfigureConfig,
    details: &InterfaceDetails,
    active: &CanActiveConfig,
) -> bool {
    let nominal = details.can.bit_timing;
    let data = details.can.data_bit_timing;
    let data_mismatch = match request.data() {
        Some(expected) => !data.is_some_and(|actual| timing_matches(expected, actual)),
        None => data.is_some(),
    };
    request.mode() == active.mode()
        && nominal.is_some_and(|actual| timing_matches(request.nominal(), actual))
        && !data_mismatch
        && details
            .can
            .ctrl_mode
            .is_some_and(|modes| modes.has_mode(CanCtrlMode::Fd))
            == (request.mode() == CanMode::Fd)
        && request.listen_only() == active.listen_only()
        && request.loopback() == active.loopback()
        && details.can.restart_ms == Some(request.restart_ms().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> CanConfigureConfig {
        CanConfigureConfig::new_with_restart(
            CanMode::Classic,
            CanBitTiming::new(500_000, Some(875), Some(2)).expect("valid timing"),
            None,
            false,
            false,
            Some(100),
        )
        .expect("valid Classical CAN configuration")
    }

    fn configured_details(request: &CanConfigureConfig) -> InterfaceDetails {
        InterfaceDetails {
            is_up: true,
            mtu: Some(Mtu::Standard),
            can: InterfaceCanParams {
                bit_timing: Some(to_socketcan_timing(request.nominal())),
                ctrl_mode: Some(CanCtrlModes::default()),
                restart_ms: Some(request.restart_ms().unwrap_or(0)),
                ..InterfaceCanParams::default()
            },
            ..InterfaceDetails::default()
        }
    }

    fn active(request: &CanConfigureConfig) -> CanActiveConfig {
        CanActiveConfig::new(
            request.mode(),
            *request.nominal(),
            request.data().copied(),
            request.listen_only(),
            request.loopback(),
            CLOCK_DOMAIN,
        )
        .expect("valid active configuration")
    }

    #[test]
    fn classic_verification_accepts_absent_data_timing() {
        let request = request();
        let details = configured_details(&request);
        assert!(config_matches(
            &request,
            &details,
            &active(&request)
        ));

        let mut unexpected_data = details;
        unexpected_data.can.data_bit_timing = Some(to_socketcan_timing(request.nominal()));
        assert!(!config_matches(
            &request,
            &unexpected_data,
            &active(&request)
        ));
    }

    #[test]
    fn verification_checks_sample_point_sjw_and_restart_delay() {
        let request = request();
        let active = active(&request);

        let mut wrong_sample_point = configured_details(&request);
        wrong_sample_point
            .can
            .bit_timing
            .as_mut()
            .expect("nominal timing")
            .sample_point = 874;
        assert!(!config_matches(&request, &wrong_sample_point, &active));

        let mut wrong_sjw = configured_details(&request);
        wrong_sjw
            .can
            .bit_timing
            .as_mut()
            .expect("nominal timing")
            .sjw = 1;
        assert!(!config_matches(&request, &wrong_sjw, &active));

        let mut wrong_restart = configured_details(&request);
        wrong_restart.can.restart_ms = Some(99);
        assert!(!config_matches(&request, &wrong_restart, &active));
    }

    #[test]
    fn fingerprint_tracks_full_timing_control_modes_and_termination() {
        let mut details = InterfaceDetails {
            name: Some("can-test".to_owned()),
            index: 7,
            ..InterfaceDetails::default()
        };
        details.can.bit_timing = Some(socketcan::nl::CanBitTiming {
            bitrate: 500_000,
            sample_point: 875,
            tq: 125,
            prop_seg: 2,
            phase_seg1: 4,
            phase_seg2: 1,
            sjw: 1,
            brp: 2,
        });
        let identity = ResourceId::parse("can:path:test").expect("valid test identity");
        let baseline = fingerprint_for_identity(&details, identity.clone());

        let timing_mutations: [fn(&mut socketcan::nl::CanBitTiming); 8] = [
            |timing: &mut socketcan::nl::CanBitTiming| timing.bitrate += 1,
            |timing: &mut socketcan::nl::CanBitTiming| timing.sample_point += 1,
            |timing: &mut socketcan::nl::CanBitTiming| timing.tq += 1,
            |timing: &mut socketcan::nl::CanBitTiming| timing.prop_seg += 1,
            |timing: &mut socketcan::nl::CanBitTiming| timing.phase_seg1 += 1,
            |timing: &mut socketcan::nl::CanBitTiming| timing.phase_seg2 += 1,
            |timing: &mut socketcan::nl::CanBitTiming| timing.sjw += 1,
            |timing: &mut socketcan::nl::CanBitTiming| timing.brp += 1,
        ];
        for mutate in timing_mutations {
            let mut changed = details.clone();
            mutate(changed.can.bit_timing.as_mut().expect("nominal timing"));
            assert_ne!(
                fingerprint_for_identity(&changed, identity.clone()),
                baseline
            );
        }

        for mode in control_modes() {
            let mut changed = details.clone();
            let mut modes = CanCtrlModes::default();
            modes.add(mode, true);
            changed.can.ctrl_mode = Some(modes);
            assert_ne!(
                fingerprint_for_identity(&changed, identity.clone()),
                baseline
            );
        }

        let mut changed = details.clone();
        changed.can.termination = Some(120);
        assert_ne!(
            fingerprint_for_identity(&changed, identity.clone()),
            baseline
        );

        changed = details.clone();
        changed.index += 1;
        assert_ne!(
            fingerprint_for_identity(&changed, identity.clone()),
            baseline
        );

        changed = details.clone();
        changed.name = Some("renamed".to_owned());
        assert_ne!(
            fingerprint_for_identity(&changed, identity.clone()),
            baseline
        );

        let other_identity =
            ResourceId::parse("can:path:other").expect("valid alternate test identity");
        assert_ne!(fingerprint_for_identity(&details, other_identity), baseline);
    }

    #[test]
    fn restore_omits_control_modes_without_successful_write_evidence() {
        let details = InterfaceDetails::default();
        let params = restore_params(&details, RestoreEvidence::default());

        assert!(params.ctrl_mode.is_none());
    }

    #[test]
    fn configure_restore_evidence_only_marks_changed_control_modes() {
        let mut details = InterfaceDetails::default();
        let mut snapshot_modes = CanCtrlModes::default();
        snapshot_modes.add(CanCtrlMode::Loopback, true);
        details.can.ctrl_mode = Some(snapshot_modes);
        let restore_evidence = restore_evidence_for_configure(&details, &request());

        assert_eq!(restore_evidence.control_modes, [false, false, true]);
    }

    #[test]
    fn restore_only_targets_control_modes_with_support_evidence() {
        let mut details = InterfaceDetails::default();
        let mut snapshot_modes = CanCtrlModes::default();
        for mode in control_modes() {
            snapshot_modes.add(mode, true);
        }
        details.can.ctrl_mode = Some(snapshot_modes);

        let modes = restore_params(
            &details,
            RestoreEvidence {
                control_modes: [false, false, true],
            },
        )
        .ctrl_mode
        .expect("changed control mode supplies support evidence");

        for (mode, expected) in [
            (CanCtrlMode::Loopback, true),
            (CanCtrlMode::ListenOnly, false),
            (CanCtrlMode::TripleSampling, false),
            (CanCtrlMode::OneShot, false),
            (CanCtrlMode::BerrReporting, false),
            (CanCtrlMode::Fd, false),
            (CanCtrlMode::PresumeAck, false),
            (CanCtrlMode::NonIso, false),
            (CanCtrlMode::CcLen8Dlc, false),
        ] {
            assert_eq!(
                modes.has_mode(mode),
                expected,
                "restore must not target control modes Configure did not mutate",
            );
        }
    }

    #[test]
    fn capabilities_require_kernel_and_nonvirtual_sysfs_evidence() {
        let mut details = InterfaceDetails::default();
        assert_eq!(capabilities_for_details(&details, false), (false, false));

        details.can.bit_timing_const = Some(Default::default());
        assert_eq!(capabilities_for_details(&details, false), (false, false));
        assert_eq!(capabilities_for_details(&details, true), (false, true));

        details.can.data_bit_timing_const = Some(Default::default());
        assert_eq!(capabilities_for_details(&details, true), (true, true));
    }
}

fn fingerprint(
    details: &InterfaceDetails,
    operation: &'static str,
    descriptor: &ResourceDescriptor,
) -> HalResult<LinkFingerprint> {
    let interface_name = details.name.as_deref().ok_or_else(|| {
        HalError::new(
            "runtime.transport.unavailable",
            ErrorCategory::Unavailable,
            operation,
            true,
            "SocketCAN netlink details omitted the interface name",
        )
        .expect("static SocketCAN error metadata is valid")
        .with_resource_id(descriptor.id().clone())
    })?;
    let metadata = metadata_from_sysfs(interface_name);
    let physical_identity = identity_from_metadata(&metadata).map_err(|error| {
        HalError::new(
            "runtime.transport.unavailable",
            ErrorCategory::Unavailable,
            operation,
            true,
            format!("SocketCAN physical identity could not be resolved: {error}"),
        )
        .expect("static SocketCAN error metadata is valid")
        .with_resource_id(descriptor.id().clone())
    })?;
    Ok(fingerprint_for_identity(details, physical_identity.id))
}

fn fingerprint_for_identity(
    details: &InterfaceDetails,
    physical_identity: ResourceId,
) -> LinkFingerprint {
    LinkFingerprint {
        interface_index: details.index,
        interface_name: details.name.clone().unwrap_or_default(),
        physical_identity,
        is_up: details.is_up,
        mtu: details.mtu,
        nominal: details.can.bit_timing.map(timing_fingerprint),
        data: details.can.data_bit_timing.map(timing_fingerprint),
        restart_ms: details.can.restart_ms,
        control_modes: control_fingerprint(details.can.ctrl_mode),
        termination: details.can.termination,
    }
}

fn verify_physical_identity(
    fingerprint: &LinkFingerprint,
    descriptor: &ResourceDescriptor,
    operation: &'static str,
) -> HalResult<()> {
    if &fingerprint.physical_identity != descriptor.id() {
        return Err(HalError::new(
            "runtime.resource.not_found",
            ErrorCategory::NotFound,
            operation,
            false,
            "SocketCAN endpoint no longer resolves to the selected physical identity",
        )?
        .with_resource_id(descriptor.id().clone()));
    }
    Ok(())
}

fn timing_matches(expected: &CanBitTiming, actual: socketcan::nl::CanBitTiming) -> bool {
    actual.bitrate == expected.bitrate()
        && expected
            .sample_point_permill()
            .is_none_or(|sample_point| actual.sample_point == u32::from(sample_point))
        && expected
            .sjw()
            .is_none_or(|sjw| actual.sjw == u32::from(sjw))
}

fn control_fingerprint(modes: Option<CanCtrlModes>) -> [bool; 9] {
    let modes = modes.unwrap_or_default();
    control_modes().map(|mode| modes.has_mode(mode))
}

fn configured_control_modes() -> [CanCtrlMode; 3] {
    [
        CanCtrlMode::Fd,
        CanCtrlMode::ListenOnly,
        CanCtrlMode::Loopback,
    ]
}

fn restore_evidence_for_configure(
    before: &InterfaceDetails,
    request: &CanConfigureConfig,
) -> RestoreEvidence {
    let before_modes = before.can.ctrl_mode.unwrap_or_default();
    let configured_modes = configured_control_modes();
    let requested = requested_control_modes(request);
    RestoreEvidence {
        control_modes: std::array::from_fn(|index| {
            before_modes.has_mode(configured_modes[index]) != requested[index]
        }),
    }
}

fn requested_control_modes(request: &CanConfigureConfig) -> [bool; 3] {
    [
        request.mode() == CanMode::Fd,
        request.listen_only(),
        request.loopback(),
    ]
}

fn control_modes() -> [CanCtrlMode; 9] {
    [
        CanCtrlMode::Loopback,
        CanCtrlMode::ListenOnly,
        CanCtrlMode::TripleSampling,
        CanCtrlMode::OneShot,
        CanCtrlMode::BerrReporting,
        CanCtrlMode::Fd,
        CanCtrlMode::PresumeAck,
        CanCtrlMode::NonIso,
        CanCtrlMode::CcLen8Dlc,
    ]
}

fn timing_fingerprint(timing: socketcan::nl::CanBitTiming) -> TimingFingerprint {
    TimingFingerprint {
        bitrate: timing.bitrate,
        sample_point: timing.sample_point,
        tq: timing.tq,
        prop_seg: timing.prop_seg,
        phase_seg1: timing.phase_seg1,
        phase_seg2: timing.phase_seg2,
        sjw: timing.sjw,
        brp: timing.brp,
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

fn from_socketcan_timing(
    timing: socketcan::nl::CanBitTiming,
    operation: &'static str,
    timing_kind: &'static str,
) -> HalResult<CanBitTiming> {
    let sample_point = nonzero_u16(timing.sample_point).ok_or_else(|| {
        invalid_kernel_timing(operation, timing_kind, "sample point exceeds u16")
    })?;
    let sjw = nonzero_u16(timing.sjw)
        .ok_or_else(|| invalid_kernel_timing(operation, timing_kind, "SJW exceeds u16"))?;
    CanBitTiming::new(timing.bitrate, sample_point, sjw).map_err(|error| {
        invalid_kernel_timing(operation, timing_kind, error.debug_message())
    })
}

fn nonzero_u16(value: u32) -> Option<Option<u16>> {
    u16::try_from(value)
        .ok()
        .map(|value| (value != 0).then_some(value))
}

fn invalid_kernel_timing(
    operation: &'static str,
    timing_kind: &'static str,
    reason: impl std::fmt::Display,
) -> HalError {
    HalError::new(
        "runtime.transport.unavailable",
        ErrorCategory::Unavailable,
        operation,
        true,
        format!("SocketCAN reported invalid {timing_kind} timing: {reason}"),
    )
    .expect("static SocketCAN error metadata is valid")
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

fn map_link_io_error(
    operation: &'static str,
    error: std::io::Error,
    descriptor: &ResourceDescriptor,
) -> HalError {
    match error.raw_os_error() {
        Some(raw_code) => {
            os_link_error(operation, raw_code, error.to_string())
                .with_resource_id(descriptor.id().clone())
        }
        None => generic_link_error(operation, error).with_resource_id(descriptor.id().clone()),
    }
}

fn map_nl_error<T: std::fmt::Debug, P: std::fmt::Debug>(
    operation: &'static str,
    error: neli::err::NlError<T, P>,
    descriptor: &ResourceDescriptor,
) -> HalError {
    map_nl_error_unscoped(operation, error).with_resource_id(descriptor.id().clone())
}

fn map_nl_error_unscoped<T: std::fmt::Debug, P: std::fmt::Debug>(
    operation: &'static str,
    error: neli::err::NlError<T, P>,
) -> HalError {
    match error {
        neli::err::NlError::Nlmsgerr(message) => {
            let raw_code = message.error.saturating_neg();
            os_link_error(operation, raw_code, message.to_string())
        }
        neli::err::NlError::Wrapped(neli::err::WrappedError::IOError(error)) => {
            let raw_code = error.raw_os_error();
            let mapped = raw_code.map_or_else(
                || generic_link_error(operation, error),
                |raw_code| os_link_error(operation, raw_code, error.to_string()),
            );
            mapped
        }
        error => generic_link_error(operation, error),
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
