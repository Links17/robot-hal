use std::fmt;

use getrandom::fill;
use seeed_hal_camera::{
    CameraFormat, CameraPixelFormat, MAX_CAMERA_FRAME_BYTES, MAX_CAMERA_HEIGHT,
    MAX_CAMERA_SLOT_COUNT, MAX_CAMERA_WIDTH, MIN_CAMERA_SLOT_COUNT,
};
use sha2::{Digest, Sha256};
use zeroize::Zeroize;

use crate::{HalResult, invalid};

pub const MAX_MAPPING_BYTES: usize = 256 * 1024 * 1024;
pub(crate) const HEADER_BYTES: usize = 192;
pub(crate) const SLOT_HEADER_BYTES: usize = 128;
pub(crate) const SLOT_ALIGNMENT: usize = 64;
pub(crate) const MAGIC: [u8; 8] = *b"SHRING04";
pub(crate) const LAYOUT_MAJOR: u16 = 1;
pub(crate) const LAYOUT_MINOR: u16 = 0;
pub(crate) const MAX_PLANES: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum PixelFormat {
    Nv12 = 1,
    Yuyv = 2,
    Mjpeg = 3,
}

impl TryFrom<u32> for PixelFormat {
    type Error = seeed_hal_core::HalError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Nv12),
            2 => Ok(Self::Yuyv),
            3 => Ok(Self::Mjpeg),
            _ => Err(invalid("shared_memory.layout", "unknown pixel format")),
        }
    }
}

impl From<CameraPixelFormat> for PixelFormat {
    fn from(value: CameraPixelFormat) -> Self {
        match value {
            CameraPixelFormat::Nv12 => Self::Nv12,
            CameraPixelFormat::Yuyv => Self::Yuyv,
            CameraPixelFormat::Mjpeg => Self::Mjpeg,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaneLayout {
    offset: u32,
    length: u32,
    stride: u32,
}

impl PlaneLayout {
    pub fn new(offset: usize, length: usize, stride: usize) -> HalResult<Self> {
        let offset = u32::try_from(offset)
            .map_err(|_| invalid("shared_memory.plane", "plane offset exceeds layout range"))?;
        let length = u32::try_from(length)
            .map_err(|_| invalid("shared_memory.plane", "plane length exceeds layout range"))?;
        let stride = u32::try_from(stride)
            .map_err(|_| invalid("shared_memory.plane", "plane stride exceeds layout range"))?;
        if length == 0 || stride == 0 {
            return Err(invalid(
                "shared_memory.plane",
                "plane length and stride must be non-zero",
            ));
        }
        offset
            .checked_add(length)
            .ok_or_else(|| invalid("shared_memory.plane", "plane range overflows"))?;
        Ok(Self {
            offset,
            length,
            stride,
        })
    }

    pub const fn offset(self) -> usize {
        self.offset as usize
    }
    pub const fn length(self) -> usize {
        self.length as usize
    }
    pub const fn stride(self) -> usize {
        self.stride as usize
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameMetadata {
    format: PixelFormat,
    width: u32,
    height: u32,
    sequence: u64,
    generation: u64,
    monotonic_timestamp_ns: u64,
    dropped_count: u64,
    planes: Vec<PlaneLayout>,
}

impl FrameMetadata {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        format: PixelFormat,
        width: u32,
        height: u32,
        sequence: u64,
        generation: u64,
        monotonic_timestamp_ns: u64,
        dropped_count: u64,
        planes: Vec<PlaneLayout>,
    ) -> HalResult<Self> {
        validate_dimensions(format, width, height)?;
        validate_planes(format, width, height, &planes, MAX_CAMERA_FRAME_BYTES)?;
        Ok(Self {
            format,
            width,
            height,
            sequence,
            generation,
            monotonic_timestamp_ns,
            dropped_count,
            planes,
        })
    }

    pub const fn format(&self) -> PixelFormat {
        self.format
    }
    pub const fn width(&self) -> u32 {
        self.width
    }
    pub const fn height(&self) -> u32 {
        self.height
    }
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    pub const fn monotonic_timestamp_ns(&self) -> u64 {
        self.monotonic_timestamp_ns
    }
    pub const fn dropped_count(&self) -> u64 {
        self.dropped_count
    }
    pub fn planes(&self) -> &[PlaneLayout] {
        &self.planes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotState {
    Free,
    Writing,
    Ready,
    Pinned,
}

impl SlotState {
    pub(crate) const fn raw(self) -> u64 {
        match self {
            Self::Free => 0,
            Self::Writing => 1,
            Self::Ready => 2,
            Self::Pinned => 3,
        }
    }

    pub(crate) fn from_raw(value: u64) -> HalResult<Self> {
        match value {
            0 => Ok(Self::Free),
            1 => Ok(Self::Writing),
            2 => Ok(Self::Ready),
            3 => Ok(Self::Pinned),
            _ => Err(invalid("shared_memory.slot", "slot state is invalid")),
        }
    }
}

pub struct MappingToken([u8; 32]);

impl MappingToken {
    pub fn generate() -> HalResult<Self> {
        let mut token = [0_u8; 32];
        fill(&mut token)
            .map_err(|error| crate::internal("shared_memory.token", error.to_string()))?;
        Ok(Self(token))
    }

    pub(crate) fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.0).into()
    }
}

impl Clone for MappingToken {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl Drop for MappingToken {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for MappingToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MappingToken(REDACTED)")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MappingIdentity([u8; 32]);

impl MappingIdentity {
    pub(crate) fn generate() -> HalResult<Self> {
        let mut identity = [0_u8; 32];
        fill(&mut identity)
            .map_err(|error| crate::internal("shared_memory.identity", error.to_string()))?;
        Ok(Self(identity))
    }

    pub(crate) const fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

pub struct MappingDescriptor {
    pub(crate) name: String,
    pub(crate) identity: MappingIdentity,
    pub(crate) token: MappingToken,
    pub(crate) total_length: usize,
}

impl MappingDescriptor {
    pub fn mapping_name(&self) -> &str {
        &self.name
    }

    pub fn mapping_identity(&self) -> &MappingIdentity {
        &self.identity
    }

    pub(crate) fn token(&self) -> &MappingToken {
        &self.token
    }

    #[cfg(test)]
    pub(crate) fn replace_token_for_test(&mut self, token: MappingToken) {
        self.token = token;
    }
}

impl Clone for MappingDescriptor {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            identity: self.identity.clone(),
            token: self.token.clone(),
            total_length: self.total_length,
        }
    }
}

impl fmt::Debug for MappingDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MappingDescriptor")
            .field("mapping_name", &self.name)
            .field("mapping_identity", &self.identity)
            .field("capability_token", &"REDACTED")
            .field("total_length", &self.total_length)
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct RingConfig {
    format: CameraFormat,
    slot_count: usize,
    payload_capacity: usize,
    slot_stride: usize,
    total_length: usize,
}

impl RingConfig {
    pub fn new(
        format: CameraFormat,
        slot_count: usize,
        payload_capacity: usize,
    ) -> HalResult<Self> {
        format.validate()?;
        if !(MIN_CAMERA_SLOT_COUNT..=MAX_CAMERA_SLOT_COUNT).contains(&slot_count) {
            return Err(invalid(
                "shared_memory.config",
                "slot count must be within the inclusive public range",
            ));
        }
        if payload_capacity > MAX_CAMERA_FRAME_BYTES
            || payload_capacity < format.worst_case_frame_bytes()?
        {
            return Err(invalid(
                "shared_memory.config",
                "payload capacity must cover the negotiated format within the frame bound",
            ));
        }
        let slot_stride = align_up(
            SLOT_HEADER_BYTES
                .checked_add(payload_capacity)
                .ok_or_else(|| invalid("shared_memory.config", "slot stride overflows"))?,
            SLOT_ALIGNMENT,
        )?;
        let total_length = HEADER_BYTES
            .checked_add(
                slot_stride
                    .checked_mul(slot_count)
                    .ok_or_else(|| invalid("shared_memory.config", "mapping length overflows"))?,
            )
            .ok_or_else(|| invalid("shared_memory.config", "mapping length overflows"))?;
        if total_length > MAX_MAPPING_BYTES {
            return Err(invalid(
                "shared_memory.config",
                "mapping length exceeds the bounded shared-memory limit",
            ));
        }
        Ok(Self {
            format,
            slot_count,
            payload_capacity,
            slot_stride,
            total_length,
        })
    }

    pub const fn slot_count(&self) -> usize {
        self.slot_count
    }
    pub const fn payload_capacity(&self) -> usize {
        self.payload_capacity
    }
    pub const fn slot_stride(&self) -> usize {
        self.slot_stride
    }
    pub const fn total_length(&self) -> usize {
        self.total_length
    }
    pub fn format(&self) -> &CameraFormat {
        &self.format
    }
}

#[derive(Clone, Debug)]
pub struct ValidatedHeader {
    config: RingConfig,
    identity: MappingIdentity,
}

impl ValidatedHeader {
    pub(crate) fn new(config: RingConfig, identity: MappingIdentity) -> Self {
        Self { config, identity }
    }

    pub fn config(&self) -> &RingConfig {
        &self.config
    }
    pub fn identity(&self) -> &MappingIdentity {
        &self.identity
    }
}

pub(crate) fn align_up(value: usize, alignment: usize) -> HalResult<usize> {
    value
        .checked_add(alignment - 1)
        .map(|value| value / alignment * alignment)
        .ok_or_else(|| invalid("shared_memory.layout", "alignment overflows"))
}

pub(crate) fn validate_dimensions(format: PixelFormat, width: u32, height: u32) -> HalResult<()> {
    if width == 0 || height == 0 || width > MAX_CAMERA_WIDTH || height > MAX_CAMERA_HEIGHT {
        return Err(invalid(
            "shared_memory.layout",
            "frame dimensions are outside the negotiated bounds",
        ));
    }
    if format == PixelFormat::Nv12 && (width % 2 != 0 || height % 2 != 0) {
        return Err(invalid(
            "shared_memory.layout",
            "NV12 dimensions must be even",
        ));
    }
    Ok(())
}

pub(crate) fn validate_planes(
    format: PixelFormat,
    width: u32,
    height: u32,
    planes: &[PlaneLayout],
    payload_length: usize,
) -> HalResult<()> {
    let expected_count = if format == PixelFormat::Nv12 { 2 } else { 1 };
    if planes.len() != expected_count || planes.len() > MAX_PLANES {
        return Err(invalid(
            "shared_memory.layout",
            "plane count does not match the pixel format",
        ));
    }
    let width = width as usize;
    let height = height as usize;
    for plane in planes {
        let end = plane
            .offset()
            .checked_add(plane.length())
            .ok_or_else(|| invalid("shared_memory.layout", "plane range overflows"))?;
        if end > payload_length {
            return Err(invalid(
                "shared_memory.layout",
                "plane range escapes the frame payload",
            ));
        }
    }
    let minimum = match format {
        PixelFormat::Nv12 => [(width, height), (width, height / 2), (0, 0)],
        PixelFormat::Yuyv => [
            (width.checked_mul(2).unwrap_or(usize::MAX), height),
            (0, 0),
            (0, 0),
        ],
        PixelFormat::Mjpeg => [(0, 0), (0, 0), (0, 0)],
    };
    for (plane, (minimum_stride, rows)) in planes.iter().zip(minimum) {
        if plane.stride() < minimum_stride
            || plane.length()
                < plane
                    .stride()
                    .checked_mul(rows)
                    .ok_or_else(|| invalid("shared_memory.layout", "plane size overflows"))?
        {
            return Err(invalid(
                "shared_memory.layout",
                "plane dimensions do not cover the negotiated frame",
            ));
        }
    }
    let mut ranges: Vec<_> = planes
        .iter()
        .map(|plane| (plane.offset(), plane.offset() + plane.length()))
        .collect();
    ranges.sort_unstable_by_key(|range| range.0);
    if ranges.windows(2).any(|ranges| ranges[0].1 > ranges[1].0) {
        return Err(invalid("shared_memory.layout", "plane ranges overlap"));
    }
    Ok(())
}
