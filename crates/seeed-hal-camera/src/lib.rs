#![forbid(unsafe_code)]

use async_trait::async_trait;
use bytes::Bytes;
use seeed_hal_core::{
    CapabilityId, ErrorCategory, HalError, HalResult, ResourceDescriptor, ResourceSelector,
};
use std::time::Duration;

pub use seeed_hal_core::{
    CapabilitySet, Endpoint, IdentityQuality, ResourceId, ResourceProperties, TransportKind,
};

pub const MAX_CAMERA_WIDTH: u32 = 4096;
pub const MAX_CAMERA_HEIGHT: u32 = 2160;
pub const MAX_CAMERA_FRAME_BYTES: usize = 24 * 1024 * 1024;
pub const DEFAULT_CAMERA_SLOT_COUNT: usize = 4;
pub const MIN_CAMERA_SLOT_COUNT: usize = 4;
pub const MAX_CAMERA_SLOT_COUNT: usize = 8;

pub const CAMERA_CAPTURE_CAPABILITY: &str = "camera.capture/v1";
pub const CAMERA_FRAMES_SHM_CAPABILITY: &str = "camera.frames.shm/v1";
pub const CAMERA_CONTROLS_CAPABILITY: &str = "camera.controls/v1";

pub fn camera_capture_capability() -> CapabilityId {
    CapabilityId::parse(CAMERA_CAPTURE_CAPABILITY).expect("static camera capability is valid")
}

pub fn camera_frames_shm_capability() -> CapabilityId {
    CapabilityId::parse(CAMERA_FRAMES_SHM_CAPABILITY).expect("static camera capability is valid")
}

pub fn camera_controls_capability() -> CapabilityId {
    CapabilityId::parse(CAMERA_CONTROLS_CAPABILITY).expect("static camera capability is valid")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CameraPixelFormat {
    Nv12,
    Yuyv,
    Mjpeg,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraFormat {
    pixel_format: CameraPixelFormat,
    width: u32,
    height: u32,
}

impl CameraFormat {
    pub fn new(pixel_format: CameraPixelFormat, width: u32, height: u32) -> HalResult<Self> {
        let format = Self {
            pixel_format,
            width,
            height,
        };
        format.validate()?;
        Ok(format)
    }

    pub const fn pixel_format(&self) -> CameraPixelFormat {
        self.pixel_format
    }

    pub const fn width(&self) -> u32 {
        self.width
    }

    pub const fn height(&self) -> u32 {
        self.height
    }

    pub fn worst_case_frame_bytes(&self) -> HalResult<usize> {
        let pixels = usize::try_from(self.width)
            .ok()
            .and_then(|width| {
                usize::try_from(self.height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or_else(|| invalid_format("camera.format", "camera dimensions overflow"))?;
        match self.pixel_format {
            CameraPixelFormat::Nv12 => pixels
                .checked_mul(3)
                .and_then(|bytes| bytes.checked_div(2))
                .ok_or_else(|| invalid_format("camera.format", "NV12 frame size overflows")),
            CameraPixelFormat::Yuyv => pixels
                .checked_mul(2)
                .ok_or_else(|| invalid_format("camera.format", "YUYV frame size overflows")),
            CameraPixelFormat::Mjpeg => Ok(MAX_CAMERA_FRAME_BYTES),
        }
    }

    pub fn validate(&self) -> HalResult<()> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_CAMERA_WIDTH
            || self.height > MAX_CAMERA_HEIGHT
        {
            return Err(invalid_format(
                "camera.format",
                "camera dimensions must be non-zero and within public bounds",
            ));
        }
        if self.worst_case_frame_bytes()? > MAX_CAMERA_FRAME_BYTES {
            return Err(invalid_format(
                "camera.format",
                "camera format worst-case payload exceeds the frame bound",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraRequest {
    format: CameraFormat,
    slot_count: usize,
}

impl CameraRequest {
    pub fn new(format: CameraFormat, slot_count: usize) -> HalResult<Self> {
        format.validate()?;
        if !(MIN_CAMERA_SLOT_COUNT..=MAX_CAMERA_SLOT_COUNT).contains(&slot_count) {
            return Err(HalError::new(
                "camera.request.invalid",
                ErrorCategory::InvalidArgument,
                "camera.request",
                false,
                "camera slot count must be within the inclusive public range",
            )?);
        }
        Ok(Self { format, slot_count })
    }

    pub fn format(&self) -> &CameraFormat {
        &self.format
    }

    pub const fn slot_count(&self) -> usize {
        self.slot_count
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CameraPlaneLayout {
    offset: usize,
    length: usize,
    stride: usize,
}

impl CameraPlaneLayout {
    pub fn new(offset: usize, length: usize, stride: usize) -> HalResult<Self> {
        if length == 0 || stride == 0 {
            return Err(HalError::new(
                "camera.frame.invalid",
                ErrorCategory::InvalidArgument,
                "camera.plane_layout",
                false,
                "camera plane length and stride must be non-zero",
            )?);
        }
        offset
            .checked_add(length)
            .ok_or_else(|| invalid_frame("camera.plane_layout", "camera plane range overflows"))?;
        Ok(Self {
            offset,
            length,
            stride,
        })
    }

    pub const fn offset(&self) -> usize {
        self.offset
    }
    pub const fn length(&self) -> usize {
        self.length
    }
    pub const fn stride(&self) -> usize {
        self.stride
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraFrameMetadata {
    format: CameraFormat,
    planes: Vec<CameraPlaneLayout>,
    sequence: u64,
    monotonic_timestamp_ns: u64,
    clock_domain: String,
    dropped_count: u64,
}

impl CameraFrameMetadata {
    pub fn new(
        format: CameraFormat,
        planes: Vec<CameraPlaneLayout>,
        sequence: u64,
        monotonic_timestamp_ns: u64,
        clock_domain: impl Into<String>,
        dropped_count: u64,
    ) -> HalResult<Self> {
        format.validate()?;
        let clock_domain = clock_domain.into();
        if clock_domain.is_empty() || !clock_domain.is_ascii() || clock_domain.len() > 255 {
            return Err(HalError::new(
                "camera.frame.invalid",
                ErrorCategory::InvalidArgument,
                "camera.frame_metadata",
                false,
                "camera clock domain must be non-empty ASCII of at most 255 bytes",
            )?);
        }
        if planes.is_empty() {
            return Err(invalid_frame(
                "camera.frame_metadata",
                "camera frames must declare one or more planes",
            ));
        }
        Ok(Self {
            format,
            planes,
            sequence,
            monotonic_timestamp_ns,
            clock_domain,
            dropped_count,
        })
    }

    pub fn format(&self) -> &CameraFormat {
        &self.format
    }
    pub fn planes(&self) -> &[CameraPlaneLayout] {
        &self.planes
    }
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    pub const fn monotonic_timestamp_ns(&self) -> u64 {
        self.monotonic_timestamp_ns
    }
    pub fn clock_domain(&self) -> &str {
        &self.clock_domain
    }
    pub const fn dropped_count(&self) -> u64 {
        self.dropped_count
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraFrame {
    metadata: CameraFrameMetadata,
    payload: Bytes,
}

impl CameraFrame {
    pub fn new(metadata: CameraFrameMetadata, payload: Bytes) -> HalResult<Self> {
        if payload.len() > MAX_CAMERA_FRAME_BYTES
            || metadata.planes().iter().any(|plane| {
                plane
                    .offset()
                    .checked_add(plane.length())
                    .is_none_or(|end| end > payload.len())
            })
        {
            return Err(invalid_frame(
                "camera.frame",
                "camera frame payload or plane layout exceeds public bounds",
            ));
        }
        Ok(Self { metadata, payload })
    }

    pub fn metadata(&self) -> &CameraFrameMetadata {
        &self.metadata
    }
    pub fn payload(&self) -> &Bytes {
        &self.payload
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CameraControlKind {
    Exposure,
    Gain,
    WhiteBalance,
    Focus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CameraControlValue {
    Integer(i64),
    Enum(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CameraControlValues {
    Range {
        minimum: i64,
        maximum: i64,
        step: i64,
    },
    Enumerated(Vec<CameraControlValue>),
}

impl CameraControlValues {
    pub fn range(minimum: i64, maximum: i64, step: i64) -> HalResult<Self> {
        if minimum > maximum || step <= 0 {
            return Err(invalid_control_descriptor(
                "camera.control_values",
                "camera control range must be ordered with a positive step",
            ));
        }
        Ok(Self::Range {
            minimum,
            maximum,
            step,
        })
    }

    pub fn enumerated(values: Vec<CameraControlValue>) -> HalResult<Self> {
        if values.is_empty() {
            return Err(invalid_control_descriptor(
                "camera.control_values",
                "camera enumerated controls must have one or more values",
            ));
        }
        Ok(Self::Enumerated(values))
    }

    pub fn contains(&self, value: &CameraControlValue) -> bool {
        match (self, value) {
            (
                Self::Range {
                    minimum,
                    maximum,
                    step,
                },
                CameraControlValue::Integer(value),
            ) => value >= minimum && value <= maximum && (value - minimum) % step == 0,
            (Self::Enumerated(values), value) => values.contains(value),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraControlDescriptor {
    kind: CameraControlKind,
    readable: bool,
    writable: bool,
    auto_supported: bool,
    values: CameraControlValues,
    current_value_available: bool,
    diagnostic: Option<String>,
}

impl CameraControlDescriptor {
    pub fn new(
        kind: CameraControlKind,
        readable: bool,
        writable: bool,
        auto_supported: bool,
        values: CameraControlValues,
        current_value_available: bool,
        diagnostic: Option<String>,
    ) -> HalResult<Self> {
        if !readable && current_value_available {
            return Err(invalid_control_descriptor(
                "camera.control_descriptor",
                "unreadable controls cannot advertise a current value",
            ));
        }
        Ok(Self {
            kind,
            readable,
            writable,
            auto_supported,
            values,
            current_value_available,
            diagnostic,
        })
    }

    pub const fn kind(&self) -> CameraControlKind {
        self.kind
    }
    pub const fn readable(&self) -> bool {
        self.readable
    }
    pub const fn writable(&self) -> bool {
        self.writable
    }
    pub const fn auto_supported(&self) -> bool {
        self.auto_supported
    }
    pub fn values(&self) -> &CameraControlValues {
        &self.values
    }
    pub const fn current_value_available(&self) -> bool {
        self.current_value_available
    }
    pub fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }
}

#[async_trait]
pub trait CameraAdapter: Send + Sync {
    fn adapter_name(&self) -> &'static str;
    async fn enumerate(&self) -> HalResult<Vec<ResourceDescriptor>>;
    async fn open(
        &self,
        selector: &ResourceSelector,
        request: &CameraRequest,
    ) -> HalResult<Box<dyn CameraCaptureSession>>;
}

#[async_trait]
pub trait CameraCaptureSession: Send {
    fn descriptor(&self) -> &ResourceDescriptor;
    fn format(&self) -> &CameraFormat;
    async fn capture(&mut self, timeout: Duration) -> HalResult<CameraFrame>;
    async fn controls(&mut self) -> HalResult<Vec<CameraControlDescriptor>>;
    async fn get_control(&mut self, kind: CameraControlKind) -> HalResult<CameraControlValue>;
    async fn set_control(
        &mut self,
        kind: CameraControlKind,
        value: CameraControlValue,
    ) -> HalResult<()>;
    async fn set_auto(&mut self, kind: CameraControlKind, enabled: bool) -> HalResult<()>;
    async fn close(&mut self) -> HalResult<()>;
}

fn invalid_format(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "camera.format.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static camera format error metadata is valid")
}

fn invalid_frame(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "camera.frame.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static camera frame error metadata is valid")
}

fn invalid_control_descriptor(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "camera.control.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static camera control error metadata is valid")
}
