use std::ffi::{c_char, c_void};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use can_hal::SamplePoint;
use can_hal_pcan::{
    Classic, ClassicBitrate, Fd, PcanBusType, PcanChannel as BackendChannel,
    PcanDriver, PcanError, PcanFdTiming, PcanPhaseTiming, PCAN_CLOCK_HZ,
};
use libloading::Library;
use seeed_hal_can::{
    CanActiveConfig, CanBitTiming, CanBusState, CanBusStatus, CanChannel, CanFrame,
    CanId, CanLinkExpectation, CanMode, CanOpenConfig, CanTimestamp,
    CanTimestampSource, ReceivedCanFrame,
};
use seeed_hal_core::{ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceId};

const PCAN_NONEBUS: u16 = 0;
const PCAN_ATTACHED_CHANNELS_COUNT: u8 = 0x2A;
const PCAN_ATTACHED_CHANNELS: u8 = 0x2B;
const PCAN_BUSSPEED_NOMINAL: u8 = 0x1A;
const PCAN_BUSSPEED_DATA: u8 = 0x1B;
const PCAN_BITRATE_INFO_FD: u8 = 0x19;
const PCAN_LISTEN_ONLY: u8 = 0x08;
const PCAN_ALLOW_RTR_FRAMES: u8 = 0x1F;

const PCAN_PARAMETER_OFF: u32 = 0;
const PCAN_PARAMETER_ON: u32 = 1;
const FEATURE_FD_CAPABLE: u32 = 0x01;

const PCAN_MESSAGE_RTR: u8 = 0x01;
const PCAN_MESSAGE_EXTENDED: u8 = 0x02;
const PCAN_MESSAGE_FD: u8 = 0x04;
const PCAN_MESSAGE_BRS: u8 = 0x08;
const PCAN_MESSAGE_ESI: u8 = 0x10;
const PCAN_MESSAGE_ERRFRAME: u8 = 0x40;
const PCAN_MESSAGE_STATUS: u8 = 0x80;

const PCAN_ERROR_OK: u32 = 0x00000;
const PCAN_ERROR_XMTFULL: u32 = 0x00001;
const PCAN_ERROR_OVERRUN: u32 = 0x00002;
const PCAN_ERROR_BUSLIGHT: u32 = 0x00004;
const PCAN_ERROR_BUSHEAVY: u32 = 0x00008;
const PCAN_ERROR_BUSOFF: u32 = 0x00010;
const PCAN_ERROR_QRCVEMPTY: u32 = 0x00020;
const PCAN_ERROR_QOVERRUN: u32 = 0x00040;
const PCAN_ERROR_QXMTFULL: u32 = 0x00080;
const PCAN_ERROR_REGTEST: u32 = 0x00100;
const PCAN_ERROR_NODRIVER: u32 = 0x00200;
const PCAN_ERROR_HWINUSE: u32 = 0x00400;
const PCAN_ERROR_NETINUSE: u32 = 0x00800;
const PCAN_ERROR_ILLHW: u32 = 0x01400;
const PCAN_ERROR_ILLNET: u32 = 0x01800;
const PCAN_ERROR_ILLCLIENT: u32 = 0x01C00;
const PCAN_ERROR_RESOURCE: u32 = 0x02000;
const PCAN_ERROR_ILLPARAMTYPE: u32 = 0x04000;
const PCAN_ERROR_ILLPARAMVAL: u32 = 0x08000;
const PCAN_ERROR_ILLDATA: u32 = 0x20000;
const PCAN_ERROR_BUSPASSIVE: u32 = 0x40000;
const PCAN_ERROR_INITIALIZE: u32 = 0x4000000;
const PCAN_ERROR_ILLOPERATION: u32 = 0x8000000;

const MAX_HARDWARE_NAME: usize = 33;
const MAX_FD_BITRATE_STRING: usize = 256;
const MAX_SKIPPED_DIAGNOSTICS: usize = 64;
const RECEIVE_POLL_INTERVAL: Duration = Duration::from_millis(1);
const CLOCK_DOMAIN: &str = "pcan-basic";

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DriverDevice {
    pub(crate) handle: u16,
    pub(crate) device_type: u8,
    pub(crate) controller_number: u8,
    pub(crate) device_name: Option<String>,
    pub(crate) device_id: Option<u32>,
    pub(crate) channel_condition: u32,
    pub(crate) fd_capable: bool,
}

#[derive(Debug)]
pub(crate) enum DriverError {
    Unavailable(String),
    Status(u32),
    Unsupported(String),
    UnsupportedFrame(String),
    ConfigurationMismatch(String),
    InvalidFrame(String),
    Closed,
    Platform(String),
}

impl fmt::Display for DriverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message)
            | Self::Unsupported(message)
            | Self::UnsupportedFrame(message)
            | Self::ConfigurationMismatch(message)
            | Self::InvalidFrame(message)
            | Self::Platform(message) => formatter.write_str(message),
            Self::Closed => formatter.write_str("PCAN channel is closed"),
            Self::Status(status) => write!(formatter, "PCAN status 0x{status:08X}"),
        }
    }
}

impl std::error::Error for DriverError {}

pub(crate) trait Driver: Send + Sync {
    fn discover(&self) -> Result<Vec<DriverDevice>, DriverError>;

    fn open(
        &self,
        device: &DriverDevice,
        config: &CanOpenConfig,
    ) -> Result<(Box<dyn DriverChannel>, CanActiveConfig), DriverError>;
}

/// Synchronous vendor operations owned by the runtime's dedicated CAN actor.
///
/// The adapter adds no queue: PCAN-Basic queue-full/overrun statuses surface
/// immediately, and receive polling is capped by the caller's finite timeout.
pub(crate) trait DriverChannel: Send {
    fn receive(&mut self, timeout: Duration) -> Result<Option<ReceivedCanFrame>, DriverError>;
    fn send(&mut self, frame: &CanFrame) -> Result<(), DriverError>;
    fn bus_status(&mut self) -> Result<CanBusStatus, DriverError>;
    fn close(&mut self) -> Result<(), DriverError>;
}

pub(crate) struct RealDriver {
    driver: PcanDriver,
    raw: Arc<RawLibrary>,
}

impl RealDriver {
    pub(crate) fn load() -> Result<Self, DriverError> {
        let driver = PcanDriver::new().map_err(DriverError::from)?;
        let raw = RawLibrary::load(default_library_name())?;
        Ok(Self {
            driver,
            raw: Arc::new(raw),
        })
    }

    #[cfg(test)]
    fn load_from(path: &str) -> Result<Self, DriverError> {
        let driver = PcanDriver::with_library_path(path).map_err(DriverError::from)?;
        let raw = RawLibrary::load(path)?;
        Ok(Self {
            driver,
            raw: Arc::new(raw),
        })
    }
}

impl Driver for RealDriver {
    fn discover(&self) -> Result<Vec<DriverDevice>, DriverError> {
        self.raw.attached_channels()
    }

    fn open(
        &self,
        device: &DriverDevice,
        config: &CanOpenConfig,
    ) -> Result<(Box<dyn DriverChannel>, CanActiveConfig), DriverError> {
        let prepared = match config {
            CanOpenConfig::Attach(expectation) => {
                prepare_attach(self.raw.as_ref(), device, expectation)?
            }
            CanOpenConfig::Configure(request) => prepare_configure(request)?,
        };
        if prepared.mode() == CanMode::Fd && !device.fd_capable {
            return Err(DriverError::Unsupported(
                "selected PCAN channel does not support CAN FD".to_owned(),
            ));
        }

        self.raw
            .set_u32(
                device.handle,
                PCAN_LISTEN_ONLY,
                if prepared.active().listen_only() {
                    PCAN_PARAMETER_ON
                } else {
                    PCAN_PARAMETER_OFF
                },
            )?;
        let (bus_type, index) = bus_and_index(device.handle).ok_or_else(|| {
            DriverError::Unsupported(format!(
                "PCAN handle 0x{:04X} is outside can-hal-pcan USB/PCI/LAN ranges",
                device.handle
            ))
        })?;
        let builder = self
            .driver
            .channel_on_bus(bus_type, index)
            .map_err(DriverError::from)?;
        let owner = match prepared.kind {
            PreparedKind::Classic(bitrate) => Connected::Classic(
                builder.classic(bitrate).connect().map_err(DriverError::from)?,
            ),
            PreparedKind::Fd(timing) => Connected::Fd(
                builder
                    .fd_explicit(timing)
                    .connect()
                    .map_err(DriverError::from)?,
            ),
        };
        if let Err(error) = self.raw.set_u32(
            device.handle,
            PCAN_ALLOW_RTR_FRAMES,
            PCAN_PARAMETER_ON,
        ) {
            drop(owner);
            return Err(error);
        }
        let active = prepared.active;
        let mode = owner.mode();
        let channel = RealChannel {
            raw: Arc::clone(&self.raw),
            handle: device.handle,
            mode,
            owner: Some(owner),
        };
        Ok((Box::new(channel), active))
    }
}

struct RealChannel {
    raw: Arc<RawLibrary>,
    handle: u16,
    mode: CanMode,
    owner: Option<Connected>,
}

enum Connected {
    Classic(BackendChannel<Classic>),
    Fd(BackendChannel<Fd>),
}

impl Connected {
    fn mode(&self) -> CanMode {
        match self {
            Self::Classic(channel) => {
                let _ = channel;
                CanMode::Classic
            }
            Self::Fd(channel) => {
                let _ = channel;
                CanMode::Fd
            }
        }
    }
}

impl DriverChannel for RealChannel {
    fn receive(&mut self, timeout: Duration) -> Result<Option<ReceivedCanFrame>, DriverError> {
        if self.owner.is_none() {
            return Err(DriverError::Closed);
        }
        let deadline = Instant::now() + timeout;
        loop {
            for _ in 0..MAX_SKIPPED_DIAGNOSTICS {
                let received = match self.mode {
                    CanMode::Classic => self.raw.read_classic(self.handle)?,
                    CanMode::Fd => self.raw.read_fd(self.handle)?,
                };
                match received {
                    RawReceive::Frame(frame, timestamp_ns) => {
                        let timestamp = CanTimestamp::new(
                            timestamp_ns,
                            CanTimestampSource::HostMonotonic,
                            CLOCK_DOMAIN,
                        )
                        .map_err(|error| DriverError::InvalidFrame(error.to_string()))?;
                        return Ok(Some(ReceivedCanFrame::new(frame, Some(timestamp))));
                    }
                    RawReceive::Skipped => continue,
                    RawReceive::Empty => break,
                }
            }
            if timeout.is_zero() || Instant::now() >= deadline {
                return Ok(None);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(remaining.min(RECEIVE_POLL_INTERVAL));
        }
    }

    fn send(&mut self, frame: &CanFrame) -> Result<(), DriverError> {
        if self.owner.is_none() {
            return Err(DriverError::Closed);
        }
        frame
            .validate()
            .map_err(|error| DriverError::InvalidFrame(error.to_string()))?;
        match self.mode {
            CanMode::Classic => self.raw.write_classic(self.handle, frame),
            CanMode::Fd => self.raw.write_fd(self.handle, frame),
        }
    }

    fn bus_status(&mut self) -> Result<CanBusStatus, DriverError> {
        if self.owner.is_none() {
            return Err(DriverError::Closed);
        }
        let status = self.raw.status(self.handle);
        let state = if status & PCAN_ERROR_BUSOFF != 0 {
            CanBusState::BusOff
        } else if status & PCAN_ERROR_BUSPASSIVE != 0 {
            CanBusState::Passive
        } else if status & PCAN_ERROR_BUSHEAVY != 0 {
            CanBusState::Warning
        } else if status & PCAN_ERROR_BUSLIGHT != 0 {
            CanBusState::Warning
        } else if status == PCAN_ERROR_OK {
            CanBusState::Active
        } else {
            return Err(DriverError::Status(status));
        };
        // PCAN-Basic does not provide portable TX/RX error counters across
        // supported devices. Absence is represented honestly instead of zero.
        Ok(CanBusStatus::new(state, None, None))
    }

    fn close(&mut self) -> Result<(), DriverError> {
        self.owner.take();
        Ok(())
    }
}

impl Drop for RealChannel {
    fn drop(&mut self) {
        self.owner.take();
    }
}

pub(crate) struct NativePcanChannel {
    descriptor: ResourceDescriptor,
    active: CanActiveConfig,
    backend: Box<dyn DriverChannel>,
    closed: bool,
}

impl NativePcanChannel {
    pub(crate) fn new(
        descriptor: ResourceDescriptor,
        active: CanActiveConfig,
        backend: Box<dyn DriverChannel>,
    ) -> Self {
        Self {
            descriptor,
            active,
            backend,
            closed: false,
        }
    }

    fn ensure_open(&self, operation: &'static str) -> HalResult<()> {
        if self.closed {
            Err(closed(operation, &self.descriptor))
        } else {
            Ok(())
        }
    }
}

impl CanChannel for NativePcanChannel {
    fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    fn active_config(&self) -> &CanActiveConfig {
        &self.active
    }

    fn receive(&mut self, timeout: Duration) -> HalResult<Option<ReceivedCanFrame>> {
        self.ensure_open("can.receive")?;
        self.backend.receive(timeout).map_err(|error| {
            map_driver_error("can.receive", error, Some(self.descriptor.id()))
        })
    }

    fn send(&mut self, frame: &CanFrame) -> HalResult<()> {
        self.ensure_open("can.send")?;
        self.backend.send(frame).map_err(|error| {
            map_driver_error("can.send", error, Some(self.descriptor.id()))
        })
    }

    fn bus_status(&mut self) -> HalResult<CanBusStatus> {
        self.ensure_open("can.status")?;
        self.backend.bus_status().map_err(|error| {
            map_driver_error("can.status", error, Some(self.descriptor.id()))
        })
    }

    fn close(&mut self) -> HalResult<()> {
        if self.closed {
            return Ok(());
        }
        self.backend.close().map_err(|error| {
            map_driver_error("can.close", error, Some(self.descriptor.id()))
        })?;
        self.closed = true;
        Ok(())
    }
}

impl Drop for NativePcanChannel {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.backend.close();
            self.closed = true;
        }
    }
}

struct PreparedOpen {
    kind: PreparedKind,
    active: CanActiveConfig,
}

enum PreparedKind {
    Classic(ClassicBitrate),
    Fd(PcanFdTiming),
}

impl PreparedOpen {
    fn active(&self) -> &CanActiveConfig {
        &self.active
    }

    fn mode(&self) -> CanMode {
        self.active.mode()
    }
}

fn prepare_configure(
    request: &seeed_hal_can::CanConfigureConfig,
) -> Result<PreparedOpen, DriverError> {
    if request.loopback() {
        return Err(DriverError::Unsupported(
            "PCAN-Basic does not expose controller loopback configuration".to_owned(),
        ));
    }
    if request.restart_ms().is_some() {
        return Err(DriverError::Unsupported(
            "PCAN-Basic auto-reset has no representable restart delay".to_owned(),
        ));
    }
    let active = CanActiveConfig::new(
        request.mode(),
        *request.nominal(),
        request.data().copied(),
        request.listen_only(),
        false,
        CLOCK_DOMAIN,
    )
    .map_err(|error| DriverError::Unsupported(error.to_string()))?;
    let kind = match request.mode() {
        CanMode::Classic => {
            if request.nominal().sample_point_permill().is_some()
                || request.nominal().sjw().is_some()
            {
                return Err(DriverError::Unsupported(
                    "PCAN Classical CAN supports only predefined bitrates, not timing overrides"
                        .to_owned(),
                ));
            }
            PreparedKind::Classic(classic_bitrate(request.nominal().bitrate()).ok_or_else(
                || {
                    DriverError::Unsupported(format!(
                        "PCAN Classical CAN cannot represent {} bit/s",
                        request.nominal().bitrate()
                    ))
                },
            )?)
        }
        CanMode::Fd => {
            let data = request.data().ok_or_else(|| {
                DriverError::Unsupported("CAN FD requires data-phase timing".to_owned())
            })?;
            PreparedKind::Fd(PcanFdTiming {
                nominal: exact_phase(request.nominal(), TimingPhase::Nominal)?,
                data: exact_phase(data, TimingPhase::Data)?,
            })
        }
    };
    Ok(PreparedOpen { kind, active })
}

fn prepare_attach(
    raw: &RawLibrary,
    device: &DriverDevice,
    expectation: &CanLinkExpectation,
) -> Result<PreparedOpen, DriverError> {
    let nominal_bitrate = raw
        .get_u32(device.handle, PCAN_BUSSPEED_NOMINAL)
        .map_err(|_| {
            DriverError::Unsupported(
                "PCAN Attach requires an already initialized, queryable channel".to_owned(),
            )
        })?;
    let data_bitrate = match raw.get_u32(device.handle, PCAN_BUSSPEED_DATA) {
        Ok(value) if value != 0 => Some(value),
        Ok(_) => None,
        Err(DriverError::Status(status))
            if status
                & (PCAN_ERROR_INITIALIZE
                    | PCAN_ERROR_ILLPARAMTYPE
                    | PCAN_ERROR_ILLPARAMVAL
                    | PCAN_ERROR_ILLOPERATION)
                != 0 =>
        {
            None
        }
        Err(error) => return Err(error),
    };
    let listen_only = raw.get_u32(device.handle, PCAN_LISTEN_ONLY)? != 0;
    let mode = if data_bitrate.is_some() {
        CanMode::Fd
    } else {
        CanMode::Classic
    };
    verify_expectation(
        expectation,
        mode,
        nominal_bitrate,
        data_bitrate,
        listen_only,
    )?;

    let (nominal, data, kind) = match data_bitrate {
        Some(_) => {
            let timing = parse_fd_timing(&raw.get_string(
                device.handle,
                PCAN_BITRATE_INFO_FD,
                MAX_FD_BITRATE_STRING,
            )?)?;
            let nominal = timing_to_public(timing.nominal)?;
            let data = timing_to_public(timing.data)?;
            if nominal.bitrate() != nominal_bitrate
                || Some(data.bitrate()) != data_bitrate
            {
                return Err(DriverError::Unsupported(
                    "PCAN FD bitrate metadata disagrees with active timing".to_owned(),
                ));
            }
            (nominal, Some(data), PreparedKind::Fd(timing))
        }
        None => {
            let bitrate = classic_bitrate(nominal_bitrate).ok_or_else(|| {
                DriverError::Unsupported(format!(
                    "active PCAN Classical bitrate {nominal_bitrate} is not representable by can-hal-pcan"
                ))
            })?;
            (
                CanBitTiming::new(nominal_bitrate, None, None)
                    .map_err(|error| DriverError::Unsupported(error.to_string()))?,
                None,
                PreparedKind::Classic(bitrate),
            )
        }
    };
    let active = CanActiveConfig::new(mode, nominal, data, listen_only, false, CLOCK_DOMAIN)
        .map_err(|error| DriverError::Unsupported(error.to_string()))?;
    Ok(PreparedOpen { kind, active })
}

fn verify_expectation(
    expectation: &CanLinkExpectation,
    mode: CanMode,
    nominal_bitrate: u32,
    data_bitrate: Option<u32>,
    listen_only: bool,
) -> Result<(), DriverError> {
    if expectation.loopback().is_some() {
        return Err(DriverError::Unsupported(
            "PCAN-Basic cannot query controller loopback state".to_owned(),
        ));
    }
    let mismatch = expectation.mode().is_some_and(|value| value != mode)
        || expectation
            .nominal_bitrate()
            .is_some_and(|value| value != nominal_bitrate)
        || expectation
            .data_bitrate()
            .is_some_and(|value| Some(value) != data_bitrate)
        || expectation
            .listen_only()
            .is_some_and(|value| value != listen_only);
    if mismatch {
        Err(DriverError::ConfigurationMismatch(
            "PCAN channel configuration does not match Attach expectations".to_owned(),
        ))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy)]
enum TimingPhase {
    Nominal,
    Data,
}

fn exact_phase(
    timing: &CanBitTiming,
    phase: TimingPhase,
) -> Result<PcanPhaseTiming, DriverError> {
    let sample_point = timing.sample_point_permill().unwrap_or(match phase {
        TimingPhase::Nominal => SamplePoint::NOMINAL_DEFAULT.per_mille(),
        TimingPhase::Data => SamplePoint::DATA_DEFAULT.per_mille(),
    });
    if !(500..=950).contains(&sample_point) {
        return Err(DriverError::Unsupported(format!(
            "PCAN sample point {sample_point} permill is outside 500..=950"
        )));
    }
    let bitrate = timing.bitrate();
    if bitrate == 0 || PCAN_CLOCK_HZ % bitrate != 0 {
        return Err(DriverError::Unsupported(format!(
            "PCAN cannot represent bitrate {bitrate} exactly at 80 MHz"
        )));
    }
    let divisor = PCAN_CLOCK_HZ / bitrate;
    let (max_tseg1, max_tseg2, preferred_tq) = match phase {
        TimingPhase::Nominal => (256, 128, 20),
        TimingPhase::Data => (32, 16, 10),
    };
    let mut best: Option<(u32, PcanPhaseTiming)> = None;
    for total_tq in 3..=(1 + max_tseg1 + max_tseg2).min(divisor) {
        if divisor % total_tq != 0 {
            continue;
        }
        let brp = divisor / total_tq;
        if brp == 0 || brp > 1024 {
            continue;
        }
        for tseg2 in 2..=max_tseg2.min(total_tq - 2) {
            let tseg1 = total_tq - 1 - tseg2;
            if tseg1 == 0 || tseg1 > max_tseg1 {
                continue;
            }
            if (1 + tseg1) * 1000 != u32::from(sample_point) * total_tq {
                continue;
            }
            let sjw = timing
                .sjw()
                .map(u32::from)
                .unwrap_or_else(|| tseg1.min(tseg2).min(4));
            if sjw == 0 || sjw > tseg1 || sjw > tseg2 {
                continue;
            }
            let candidate = PcanPhaseTiming {
                brp,
                tseg1,
                tseg2,
                sjw,
            };
            let distance = total_tq.abs_diff(preferred_tq);
            if best.as_ref().is_none_or(|(best_distance, _)| distance < *best_distance) {
                best = Some((distance, candidate));
            }
        }
    }
    best.map(|(_, timing)| timing).ok_or_else(|| {
        DriverError::Unsupported(format!(
            "PCAN cannot represent bitrate {bitrate}, sample point {sample_point} permill, and SJW {:?} exactly",
            timing.sjw()
        ))
    })
}

fn timing_to_public(timing: PcanPhaseTiming) -> Result<CanBitTiming, DriverError> {
    let total_tq = 1 + timing.tseg1 + timing.tseg2;
    let divisor = timing.brp.checked_mul(total_tq).ok_or_else(|| {
        DriverError::Unsupported("PCAN FD timing divisor overflowed".to_owned())
    })?;
    if divisor == 0 || PCAN_CLOCK_HZ % divisor != 0 {
        return Err(DriverError::Unsupported(
            "PCAN FD timing does not represent an exact bitrate".to_owned(),
        ));
    }
    let sample_numerator = (1 + timing.tseg1) * 1000;
    let sample_point = (sample_numerator % total_tq == 0)
        .then_some(sample_numerator / total_tq)
        .and_then(|value| u16::try_from(value).ok());
    CanBitTiming::new(
        PCAN_CLOCK_HZ / divisor,
        sample_point,
        u16::try_from(timing.sjw).ok(),
    )
    .map_err(|error| DriverError::Unsupported(error.to_string()))
}

fn parse_fd_timing(value: &str) -> Result<PcanFdTiming, DriverError> {
    let mut fields = std::collections::BTreeMap::new();
    for item in value.split(',') {
        if let Some((key, value)) = item.trim().split_once('=') {
            fields.insert(key.trim(), value.trim());
        }
    }
    let clock_ok = fields.get("f_clock_mhz").is_some_and(|value| *value == "80")
        || fields
            .get("f_clock")
            .is_some_and(|value| *value == "80000000");
    if !clock_ok {
        return Err(DriverError::Unsupported(
            "active PCAN FD timing does not use the supported 80 MHz clock".to_owned(),
        ));
    }
    let field = |name: &str| -> Result<u32, DriverError> {
        fields
            .get(name)
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| {
                DriverError::Unsupported(format!(
                    "active PCAN FD timing is missing or invalid: {name}"
                ))
            })
    };
    Ok(PcanFdTiming {
        nominal: PcanPhaseTiming {
            brp: field("nom_brp")?,
            tseg1: field("nom_tseg1")?,
            tseg2: field("nom_tseg2")?,
            sjw: field("nom_sjw")?,
        },
        data: PcanPhaseTiming {
            brp: field("data_brp")?,
            tseg1: field("data_tseg1")?,
            tseg2: field("data_tseg2")?,
            sjw: field("data_sjw")?,
        },
    })
}

fn classic_bitrate(bitrate: u32) -> Option<ClassicBitrate> {
    match bitrate {
        1_000_000 => Some(ClassicBitrate::Br1M),
        800_000 => Some(ClassicBitrate::Br800K),
        500_000 => Some(ClassicBitrate::Br500K),
        250_000 => Some(ClassicBitrate::Br250K),
        125_000 => Some(ClassicBitrate::Br125K),
        100_000 => Some(ClassicBitrate::Br100K),
        50_000 => Some(ClassicBitrate::Br50K),
        20_000 => Some(ClassicBitrate::Br20K),
        10_000 => Some(ClassicBitrate::Br10K),
        5_000 => Some(ClassicBitrate::Br5K),
        _ => None,
    }
}

fn bus_and_index(handle: u16) -> Option<(PcanBusType, u32)> {
    match handle {
        0x51..=0x60 => Some((PcanBusType::Usb, u32::from(handle - 0x51))),
        0x41..=0x50 => Some((PcanBusType::Pci, u32::from(handle - 0x41))),
        0x801..=0x810 => Some((PcanBusType::Lan, u32::from(handle - 0x801))),
        _ => None,
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PcanChannelInformation {
    channel_handle: u16,
    device_type: u8,
    controller_number: u8,
    device_features: u32,
    device_name: [c_char; MAX_HARDWARE_NAME],
    device_id: u32,
    channel_condition: u32,
}

impl Default for PcanChannelInformation {
    fn default() -> Self {
        Self {
            channel_handle: 0,
            device_type: 0,
            controller_number: 0,
            device_features: 0,
            device_name: [0; MAX_HARDWARE_NAME],
            device_id: 0,
            channel_condition: 0,
        }
    }
}

#[repr(C)]
struct PcanMessage {
    id: u32,
    message_type: u8,
    len: u8,
    data: [u8; 8],
}

#[repr(C)]
struct PcanMessageFd {
    id: u32,
    message_type: u8,
    dlc: u8,
    data: [u8; 64],
}

#[repr(C)]
struct PcanTimestamp {
    millis: u32,
    millis_overflow: u16,
    micros: u16,
}

type FnGetValue = unsafe extern "C" fn(u16, u8, *mut c_void, u32) -> u32;
type FnSetValue = unsafe extern "C" fn(u16, u8, *mut c_void, u32) -> u32;
type FnGetStatus = unsafe extern "C" fn(u16) -> u32;
type FnRead = unsafe extern "C" fn(u16, *mut PcanMessage, *mut PcanTimestamp) -> u32;
type FnReadFd = unsafe extern "C" fn(u16, *mut PcanMessageFd, *mut u64) -> u32;
type FnWrite = unsafe extern "C" fn(u16, *mut PcanMessage) -> u32;
type FnWriteFd = unsafe extern "C" fn(u16, *mut PcanMessageFd) -> u32;

// FFI safety citation: these layouts and function signatures mirror the
// audited can-hal-pcan 0.4.2 `ffi.rs`/`library.rs` PCAN-Basic 4.x boundary.
// Adapter tests exercise the safe conversion and lifecycle seam; only the
// actual vendor calls remain inside the documented SAFETY blocks below.

struct RawLibrary {
    _library: Library,
    get_value: FnGetValue,
    set_value: FnSetValue,
    get_status: FnGetStatus,
    read: FnRead,
    read_fd: FnReadFd,
    write: FnWrite,
    write_fd: FnWriteFd,
}

impl RawLibrary {
    fn load(path: &str) -> Result<Self, DriverError> {
        // SAFETY: Loading the vendor library can execute its constructors. The
        // path is the documented PCAN-Basic runtime name selected by platform.
        let library = unsafe { Library::new(path) }
            .map_err(|error| DriverError::Unavailable(error.to_string()))?;
        // SAFETY: These signatures and symbol names are the PCAN-Basic 4.x ABI;
        // the library handle is retained in this struct for every pointer use.
        let get_value = unsafe { *library.get::<FnGetValue>(b"CAN_GetValue\0") }
            .map_err(|error| DriverError::Unavailable(error.to_string()))?;
        // SAFETY: Same PCAN-Basic ABI and retained library invariant as above.
        let set_value = unsafe { *library.get::<FnSetValue>(b"CAN_SetValue\0") }
            .map_err(|error| DriverError::Unavailable(error.to_string()))?;
        // SAFETY: Same PCAN-Basic ABI and retained library invariant as above.
        let get_status = unsafe { *library.get::<FnGetStatus>(b"CAN_GetStatus\0") }
            .map_err(|error| DriverError::Unavailable(error.to_string()))?;
        // SAFETY: Same PCAN-Basic ABI and retained library invariant as above.
        let read = unsafe { *library.get::<FnRead>(b"CAN_Read\0") }
            .map_err(|error| DriverError::Unavailable(error.to_string()))?;
        // SAFETY: Same PCAN-Basic ABI and retained library invariant as above.
        let read_fd = unsafe { *library.get::<FnReadFd>(b"CAN_ReadFD\0") }
            .map_err(|error| DriverError::Unavailable(error.to_string()))?;
        // SAFETY: Same PCAN-Basic ABI and retained library invariant as above.
        let write = unsafe { *library.get::<FnWrite>(b"CAN_Write\0") }
            .map_err(|error| DriverError::Unavailable(error.to_string()))?;
        // SAFETY: Same PCAN-Basic ABI and retained library invariant as above.
        let write_fd = unsafe { *library.get::<FnWriteFd>(b"CAN_WriteFD\0") }
            .map_err(|error| DriverError::Unavailable(error.to_string()))?;
        Ok(Self {
            _library: library,
            get_value,
            set_value,
            get_status,
            read,
            read_fd,
            write,
            write_fd,
        })
    }

    fn attached_channels(&self) -> Result<Vec<DriverDevice>, DriverError> {
        let count = self.get_u32(PCAN_NONEBUS, PCAN_ATTACHED_CHANNELS_COUNT)?;
        let count = usize::try_from(count).map_err(|_| {
            DriverError::Platform("PCAN attached channel count exceeds usize".to_owned())
        })?;
        if count == 0 {
            return Ok(Vec::new());
        }
        let mut channels = vec![PcanChannelInformation::default(); count];
        let byte_len = channels
            .len()
            .checked_mul(std::mem::size_of::<PcanChannelInformation>())
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                DriverError::Platform("PCAN discovery buffer length overflowed".to_owned())
            })?;
        // SAFETY: channels is a writable contiguous buffer of byte_len bytes,
        // matching the documented TPCANChannelInformation array ABI.
        let status = unsafe {
            (self.get_value)(
                PCAN_NONEBUS,
                PCAN_ATTACHED_CHANNELS,
                channels.as_mut_ptr().cast::<c_void>(),
                byte_len,
            )
        };
        check_status(status)?;
        Ok(channels
            .into_iter()
            .filter(|channel| {
                bus_and_index(channel.channel_handle).is_some()
                    && channel.channel_condition & 0x03 != 0
            })
            .map(|channel| DriverDevice {
                handle: channel.channel_handle,
                device_type: channel.device_type,
                controller_number: channel.controller_number,
                device_name: decode_name(&channel.device_name),
                device_id: (channel.device_id != 0).then_some(channel.device_id),
                channel_condition: channel.channel_condition,
                fd_capable: channel.device_features & FEATURE_FD_CAPABLE != 0,
            })
            .collect())
    }

    fn get_u32(&self, handle: u16, parameter: u8) -> Result<u32, DriverError> {
        let mut value = 0u32;
        // SAFETY: value is a writable u32 buffer and the length matches it;
        // get_value is a resolved PCAN-Basic function retained by self.
        let status = unsafe {
            (self.get_value)(
                handle,
                parameter,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                u32::try_from(std::mem::size_of::<u32>()).expect("u32 size fits u32"),
            )
        };
        check_status(status)?;
        Ok(value)
    }

    fn get_string(
        &self,
        handle: u16,
        parameter: u8,
        capacity: usize,
    ) -> Result<String, DriverError> {
        let mut value = vec![0u8; capacity];
        let len = u32::try_from(value.len()).map_err(|_| {
            DriverError::Platform("PCAN string buffer length exceeds u32".to_owned())
        })?;
        // SAFETY: value is a writable byte buffer of len bytes and get_value
        // is a resolved PCAN-Basic function retained by self.
        let status = unsafe {
            (self.get_value)(handle, parameter, value.as_mut_ptr().cast::<c_void>(), len)
        };
        check_status(status)?;
        let end = value.iter().position(|byte| *byte == 0).unwrap_or(value.len());
        String::from_utf8(value[..end].to_vec()).map_err(|error| {
            DriverError::Platform(format!("PCAN returned non-UTF-8 timing text: {error}"))
        })
    }

    fn set_u32(&self, handle: u16, parameter: u8, value: u32) -> Result<(), DriverError> {
        let mut value = value;
        // SAFETY: value is a readable/writable u32 buffer and the length
        // matches it; set_value is retained with the loaded library.
        let status = unsafe {
            (self.set_value)(
                handle,
                parameter,
                std::ptr::from_mut(&mut value).cast::<c_void>(),
                u32::try_from(std::mem::size_of::<u32>()).expect("u32 size fits u32"),
            )
        };
        check_status(status)
    }

    fn status(&self, handle: u16) -> u32 {
        // SAFETY: get_status is retained with the loaded PCAN-Basic library
        // and handle came from vendor discovery.
        unsafe { (self.get_status)(handle) }
    }

    fn read_classic(&self, handle: u16) -> Result<RawReceive, DriverError> {
        let mut message = PcanMessage {
            id: 0,
            message_type: 0,
            len: 0,
            data: [0; 8],
        };
        let mut timestamp = PcanTimestamp {
            millis: 0,
            millis_overflow: 0,
            micros: 0,
        };
        // SAFETY: message and timestamp are writable ABI-compatible buffers;
        // handle is initialized and read is retained with the vendor library.
        let status = unsafe { (self.read)(handle, &mut message, &mut timestamp) };
        if status == PCAN_ERROR_QRCVEMPTY {
            return Ok(RawReceive::Empty);
        }
        check_status(status)?;
        let frame = from_classic_message(&message)?;
        let micros = ((u64::from(timestamp.millis_overflow) << 32)
            | u64::from(timestamp.millis))
            .saturating_mul(1_000)
            .saturating_add(u64::from(timestamp.micros));
        Ok(frame.map_or(RawReceive::Skipped, |frame| {
            RawReceive::Frame(frame, micros.saturating_mul(1_000))
        }))
    }

    fn read_fd(&self, handle: u16) -> Result<RawReceive, DriverError> {
        let mut message = PcanMessageFd {
            id: 0,
            message_type: 0,
            dlc: 0,
            data: [0; 64],
        };
        let mut timestamp = 0u64;
        // SAFETY: message and timestamp are writable ABI-compatible buffers;
        // handle is initialized and read_fd is retained with the vendor library.
        let status = unsafe { (self.read_fd)(handle, &mut message, &mut timestamp) };
        if status == PCAN_ERROR_QRCVEMPTY {
            return Ok(RawReceive::Empty);
        }
        check_status(status)?;
        Ok(from_fd_message(&message)?.map_or(RawReceive::Skipped, |frame| {
            RawReceive::Frame(frame, timestamp.saturating_mul(1_000))
        }))
    }

    fn write_classic(&self, handle: u16, frame: &CanFrame) -> Result<(), DriverError> {
        let mut message = to_classic_message(frame)?;
        // SAFETY: message is an initialized ABI-compatible buffer; handle is
        // initialized and write is retained with the vendor library.
        let status = unsafe { (self.write)(handle, &mut message) };
        check_status(status)
    }

    fn write_fd(&self, handle: u16, frame: &CanFrame) -> Result<(), DriverError> {
        let mut message = to_fd_message(frame)?;
        // SAFETY: message is an initialized ABI-compatible buffer; handle is
        // initialized and write_fd is retained with the vendor library.
        let status = unsafe { (self.write_fd)(handle, &mut message) };
        check_status(status)
    }
}

enum RawReceive {
    Frame(CanFrame, u64),
    Skipped,
    Empty,
}

fn decode_name(value: &[c_char; MAX_HARDWARE_NAME]) -> Option<String> {
    let bytes: Vec<u8> = value
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| *byte as u8)
        .collect();
    (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).trim().to_owned())
}

fn from_classic_message(message: &PcanMessage) -> Result<Option<CanFrame>, DriverError> {
    if message.message_type & (PCAN_MESSAGE_STATUS | PCAN_MESSAGE_ERRFRAME) != 0 {
        return Ok(None);
    }
    let id = from_raw_id(message.id, message.message_type)?;
    if message.message_type & PCAN_MESSAGE_RTR != 0 {
        return CanFrame::classic_remote(id, message.len).map(Some).map_err(|error| {
            DriverError::InvalidFrame(error.to_string())
        });
    }
    let len = usize::from(message.len);
    if len > message.data.len() {
        return Err(DriverError::InvalidFrame(format!(
            "PCAN Classical DLC {} exceeds 8",
            message.len
        )));
    }
    CanFrame::classic_data(id, message.data[..len].to_vec())
        .map(Some)
        .map_err(|error| DriverError::InvalidFrame(error.to_string()))
}

fn from_fd_message(message: &PcanMessageFd) -> Result<Option<CanFrame>, DriverError> {
    if message.message_type & (PCAN_MESSAGE_STATUS | PCAN_MESSAGE_ERRFRAME) != 0 {
        return Ok(None);
    }
    let id = from_raw_id(message.id, message.message_type)?;
    if message.message_type & PCAN_MESSAGE_RTR != 0 {
        return CanFrame::classic_remote(id, message.dlc).map(Some).map_err(|error| {
            DriverError::InvalidFrame(error.to_string())
        });
    }
    if message.message_type & PCAN_MESSAGE_FD != 0 {
        let len = usize::from(dlc_to_len(message.dlc));
        return CanFrame::fd_data(
            id,
            message.data[..len].to_vec(),
            message.message_type & PCAN_MESSAGE_BRS != 0,
            message.message_type & PCAN_MESSAGE_ESI != 0,
        )
        .map(Some)
        .map_err(|error| DriverError::InvalidFrame(error.to_string()));
    }
    let len = usize::from(message.dlc);
    if len > 8 {
        return Err(DriverError::InvalidFrame(format!(
            "PCAN Classical DLC {} exceeds 8",
            message.dlc
        )));
    }
    CanFrame::classic_data(id, message.data[..len].to_vec())
        .map(Some)
        .map_err(|error| DriverError::InvalidFrame(error.to_string()))
}

fn to_classic_message(frame: &CanFrame) -> Result<PcanMessage, DriverError> {
    let (id, id_type) = raw_id(frame.id().copied().ok_or_else(|| {
        DriverError::UnsupportedFrame("PCAN cannot transmit HAL error frames".to_owned())
    })?);
    let mut message = PcanMessage {
        id,
        message_type: id_type,
        len: 0,
        data: [0; 8],
    };
    match frame {
        CanFrame::ClassicData { data, .. } => {
            message.len = u8::try_from(data.len()).expect("validated Classical length fits u8");
            message.data[..data.len()].copy_from_slice(data);
        }
        CanFrame::ClassicRemote { dlc, .. } => {
            message.message_type |= PCAN_MESSAGE_RTR;
            message.len = *dlc;
        }
        CanFrame::FdData { .. } => {
            return Err(DriverError::UnsupportedFrame(
                "CAN FD frame requires an FD-configured PCAN channel".to_owned(),
            ));
        }
        CanFrame::Error { .. } => {
            return Err(DriverError::UnsupportedFrame(
                "PCAN error frames are receive diagnostics and cannot be transmitted".to_owned(),
            ));
        }
    }
    Ok(message)
}

fn to_fd_message(frame: &CanFrame) -> Result<PcanMessageFd, DriverError> {
    let (id, id_type) = raw_id(frame.id().copied().ok_or_else(|| {
        DriverError::UnsupportedFrame("PCAN cannot transmit HAL error frames".to_owned())
    })?);
    let mut message = PcanMessageFd {
        id,
        message_type: id_type,
        dlc: 0,
        data: [0; 64],
    };
    match frame {
        CanFrame::ClassicData { data, .. } => {
            message.dlc = u8::try_from(data.len()).expect("validated Classical length fits u8");
            message.data[..data.len()].copy_from_slice(data);
        }
        CanFrame::ClassicRemote { dlc, .. } => {
            message.message_type |= PCAN_MESSAGE_RTR;
            message.dlc = *dlc;
        }
        CanFrame::FdData {
            data,
            bitrate_switch,
            error_state_indicator,
            ..
        } => {
            message.message_type |= PCAN_MESSAGE_FD;
            if *bitrate_switch {
                message.message_type |= PCAN_MESSAGE_BRS;
            }
            if *error_state_indicator {
                message.message_type |= PCAN_MESSAGE_ESI;
            }
            message.dlc = len_to_dlc(data.len()).ok_or_else(|| {
                DriverError::InvalidFrame("invalid CAN FD payload length".to_owned())
            })?;
            message.data[..data.len()].copy_from_slice(data);
        }
        CanFrame::Error { .. } => {
            return Err(DriverError::UnsupportedFrame(
                "PCAN error frames are receive diagnostics and cannot be transmitted".to_owned(),
            ));
        }
    }
    Ok(message)
}

fn raw_id(id: CanId) -> (u32, u8) {
    match id {
        CanId::Standard(value) => (u32::from(value), 0),
        CanId::Extended(value) => (value, PCAN_MESSAGE_EXTENDED),
    }
}

fn from_raw_id(id: u32, message_type: u8) -> Result<CanId, DriverError> {
    if message_type & PCAN_MESSAGE_EXTENDED != 0 {
        CanId::extended(id).map_err(|error| DriverError::InvalidFrame(error.to_string()))
    } else {
        let id = u16::try_from(id).map_err(|_| {
            DriverError::InvalidFrame(format!("PCAN standard identifier 0x{id:X} exceeds u16"))
        })?;
        CanId::standard(id).map_err(|error| DriverError::InvalidFrame(error.to_string()))
    }
}

fn len_to_dlc(length: usize) -> Option<u8> {
    match length {
        0..=8 => u8::try_from(length).ok(),
        12 => Some(9),
        16 => Some(10),
        20 => Some(11),
        24 => Some(12),
        32 => Some(13),
        48 => Some(14),
        64 => Some(15),
        _ => None,
    }
}

fn dlc_to_len(dlc: u8) -> u8 {
    match dlc {
        0..=8 => dlc,
        9 => 12,
        10 => 16,
        11 => 20,
        12 => 24,
        13 => 32,
        14 => 48,
        _ => 64,
    }
}

fn check_status(status: u32) -> Result<(), DriverError> {
    if status == PCAN_ERROR_OK {
        Ok(())
    } else {
        Err(DriverError::Status(status))
    }
}

impl From<PcanError> for DriverError {
    fn from(error: PcanError) -> Self {
        match error {
            PcanError::LibraryLoad(error) => Self::Unavailable(error.to_string()),
            PcanError::Pcan(status) => Self::Status(status.0),
            PcanError::InvalidFrame(message) => Self::InvalidFrame(message),
            PcanError::InvalidChannel(index) => {
                Self::Unsupported(format!("invalid PCAN channel index {index}"))
            }
            PcanError::UnsupportedBitrate(bitrate) => {
                Self::Unsupported(format!("unsupported PCAN bitrate {bitrate}"))
            }
            PcanError::UnsupportedTiming(message) => Self::Unsupported(message),
            PcanError::Platform(message) => Self::Platform(message),
            other => Self::Platform(other.to_string()),
        }
    }
}

pub(crate) fn map_driver_error(
    operation: &'static str,
    error: DriverError,
    resource_id: Option<&ResourceId>,
) -> HalError {
    let (name, category, retryable, message, vendor_code) = match error {
        DriverError::Unavailable(message) => (
            "can.adapter.unavailable",
            ErrorCategory::Unavailable,
            false,
            message,
            None,
        ),
        DriverError::Unsupported(message) => (
            "can.configuration.unsupported",
            ErrorCategory::InvalidArgument,
            false,
            message,
            None,
        ),
        DriverError::UnsupportedFrame(message) => (
            "runtime.transport.unsupported_configuration",
            ErrorCategory::InvalidArgument,
            false,
            message,
            None,
        ),
        DriverError::InvalidFrame(message) => (
            "can.frame.invalid",
            ErrorCategory::InvalidArgument,
            false,
            message,
            None,
        ),
        DriverError::ConfigurationMismatch(message) => (
            "can.configuration.mismatch",
            ErrorCategory::Conflict,
            false,
            message,
            None,
        ),
        DriverError::Closed => (
            "runtime.session.closed",
            ErrorCategory::Conflict,
            false,
            "PCAN channel is closed".to_owned(),
            None,
        ),
        DriverError::Platform(message) => (
            "runtime.transport.unavailable",
            ErrorCategory::Unavailable,
            true,
            message,
            None,
        ),
        DriverError::Status(status) => {
            let (name, category, retryable) = status_decision(status, operation);
            (
                name,
                category,
                retryable,
                format!("PCAN-Basic returned status 0x{status:08X}"),
                Some(format!("0x{status:08X}")),
            )
        }
    };
    let mut mapped = HalError::new(name, category, operation, retryable, message)
        .expect("static PCAN error metadata is valid");
    if let Some(code) = vendor_code {
        mapped = mapped
            .with_vendor_code(code)
            .expect("formatted PCAN vendor code is valid ASCII");
    }
    if let Some(resource_id) = resource_id {
        mapped = mapped.with_resource_id(resource_id.clone());
    }
    mapped
}

fn status_decision(status: u32, operation: &'static str) -> (&'static str, ErrorCategory, bool) {
    if status & PCAN_ERROR_BUSOFF != 0 {
        ("can.bus.off", ErrorCategory::Unavailable, false)
    } else if status & (PCAN_ERROR_BUSPASSIVE | PCAN_ERROR_BUSHEAVY) != 0 {
        ("can.bus.passive", ErrorCategory::Unavailable, true)
    } else if status & (PCAN_ERROR_XMTFULL | PCAN_ERROR_QXMTFULL) != 0 {
        ("runtime.queue.full", ErrorCategory::Unavailable, true)
    } else if status & (PCAN_ERROR_OVERRUN | PCAN_ERROR_QOVERRUN) != 0 {
        ("can.receive.lagged", ErrorCategory::Unavailable, true)
    } else if status & PCAN_ERROR_NODRIVER != 0 {
        ("can.adapter.unavailable", ErrorCategory::Unavailable, false)
    } else if [PCAN_ERROR_ILLHW, PCAN_ERROR_ILLNET, PCAN_ERROR_ILLCLIENT]
        .into_iter()
        .any(|invalid| status & invalid == invalid)
        || status & (PCAN_ERROR_REGTEST | PCAN_ERROR_INITIALIZE) != 0
    {
        ("runtime.resource.not_found", ErrorCategory::NotFound, false)
    } else if status & (PCAN_ERROR_HWINUSE | PCAN_ERROR_NETINUSE | PCAN_ERROR_RESOURCE) != 0 {
        ("runtime.adapter.conflict", ErrorCategory::Conflict, false)
    } else if status
        & (PCAN_ERROR_ILLPARAMTYPE
            | PCAN_ERROR_ILLPARAMVAL
            | PCAN_ERROR_ILLDATA
            | PCAN_ERROR_ILLOPERATION)
        != 0
    {
        if operation == "can.send" {
            ("can.frame.invalid", ErrorCategory::InvalidArgument, false)
        } else {
            (
                "can.configuration.unsupported",
                ErrorCategory::InvalidArgument,
                false,
            )
        }
    } else {
        ("runtime.transport.unavailable", ErrorCategory::Unavailable, true)
    }
}

fn closed(operation: &'static str, descriptor: &ResourceDescriptor) -> HalError {
    HalError::new(
        "runtime.session.closed",
        ErrorCategory::Conflict,
        operation,
        false,
        "PCAN channel is closed",
    )
    .expect("static PCAN closed error metadata is valid")
    .with_resource_id(descriptor.id().clone())
}

const fn default_library_name() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "PCANBasic.dll"
    }
    #[cfg(target_os = "linux")]
    {
        "libpcanbasic.so"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_dynamic_library_is_adapter_unavailable() {
        let error = match RealDriver::load_from("__seeed_hal_missing_pcan_basic_library__") {
            Ok(_) => panic!("an impossible PCAN-Basic path must not load"),
            Err(error) => error,
        };
        let mapped = map_driver_error("can.adapter.load", error, None);

        assert_eq!(mapped.name().as_str(), "can.adapter.unavailable");
        assert_eq!(mapped.operation().as_str(), "can.adapter.load");
        assert!(!mapped.retryable());
    }

    #[test]
    fn classic_configuration_rejects_non_predefined_bitrate() {
        let request = seeed_hal_can::CanConfigureConfig::new(
            CanMode::Classic,
            CanBitTiming::new(333_333, None, None).unwrap(),
            None,
            false,
            false,
        )
        .unwrap();

        assert!(matches!(
            prepare_configure(&request),
            Err(DriverError::Unsupported(_))
        ));
    }

    #[test]
    fn fd_configuration_rejects_rounded_sample_point() {
        let request = seeed_hal_can::CanConfigureConfig::new(
            CanMode::Fd,
            CanBitTiming::new(500_000, Some(873), None).unwrap(),
            Some(CanBitTiming::new(2_000_000, Some(800), None).unwrap()),
            false,
            false,
        )
        .unwrap();

        assert!(matches!(
            prepare_configure(&request),
            Err(DriverError::Unsupported(_))
        ));
    }

    #[test]
    fn exact_fd_configuration_preserves_requested_sjw() {
        let request = seeed_hal_can::CanConfigureConfig::new(
            CanMode::Fd,
            CanBitTiming::new(500_000, Some(700), Some(4)).unwrap(),
            Some(CanBitTiming::new(2_000_000, Some(800), Some(2)).unwrap()),
            true,
            false,
        )
        .unwrap();

        let prepared = prepare_configure(&request).unwrap();
        let PreparedKind::Fd(timing) = prepared.kind else {
            panic!("expected FD timing");
        };
        assert_eq!(timing.nominal.sjw, 4);
        assert_eq!(timing.data.sjw, 2);
        assert!(prepared.active.listen_only());
    }

    #[test]
    fn frame_conversion_preserves_remote_and_fd_flags() {
        let remote = CanFrame::classic_remote(CanId::extended(0x18da_00f1).unwrap(), 8).unwrap();
        let raw_remote = to_fd_message(&remote).unwrap();
        assert_eq!(raw_remote.id, 0x18da_00f1);
        assert_ne!(raw_remote.message_type & PCAN_MESSAGE_EXTENDED, 0);
        assert_ne!(raw_remote.message_type & PCAN_MESSAGE_RTR, 0);
        assert_eq!(raw_remote.dlc, 8);

        let fd = CanFrame::fd_data(
            CanId::standard(0x123).unwrap(),
            vec![0x5a; 12],
            true,
            true,
        )
            .unwrap();
        let raw_fd = to_fd_message(&fd).unwrap();
        assert_eq!(raw_fd.dlc, 9);
        assert_ne!(raw_fd.message_type & PCAN_MESSAGE_FD, 0);
        assert_ne!(raw_fd.message_type & PCAN_MESSAGE_BRS, 0);
        assert_ne!(raw_fd.message_type & PCAN_MESSAGE_ESI, 0);
        assert_eq!(from_fd_message(&raw_fd).unwrap(), Some(fd));
    }

    #[test]
    fn documented_status_maps_to_vendor_code_and_resource() {
        let resource_id = ResourceId::parse("can:pcan:handle:0051").unwrap();
        let error = map_driver_error(
            "can.send",
            DriverError::Status(PCAN_ERROR_QXMTFULL),
            Some(&resource_id),
        );

        assert_eq!(error.name().as_str(), "runtime.queue.full");
        assert_eq!(error.vendor_code(), Some("0x00000080"));
        assert_eq!(error.resource_id(), Some(&resource_id));
        assert!(error.retryable());
    }

}
