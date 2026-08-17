use std::sync::atomic::{AtomicU64, Ordering};
use std::{fmt::Write, string::String};

use getrandom::fill;
use seeed_hal_camera::CameraFormat;

use crate::layout::{
    FrameMetadata, HEADER_BYTES, LAYOUT_MAJOR, LAYOUT_MINOR, MAGIC, MAX_PLANES, MappingDescriptor,
    MappingIdentity, PixelFormat, RingConfig, SLOT_HEADER_BYTES, SlotState, ValidatedHeader,
    validate_dimensions, validate_planes,
};
use crate::platform::Mapping;
use crate::{HalResult, internal, invalid, unavailable};

const HEADER_MAGIC: usize = 0;
const HEADER_MAJOR: usize = 8;
const HEADER_MINOR: usize = 10;
const HEADER_TOTAL_LENGTH: usize = 16;
const HEADER_SLOT_COUNT: usize = 24;
const HEADER_SLOT_STRIDE: usize = 32;
const HEADER_FORMAT: usize = 40;
const HEADER_WIDTH: usize = 44;
const HEADER_HEIGHT: usize = 48;
const HEADER_PAYLOAD_CAPACITY: usize = 56;
const HEADER_IDENTITY: usize = 64;
const HEADER_TOKEN_HASH: usize = 96;
const HEADER_DROPPED_COUNT: usize = 128;

const SLOT_STATE: usize = 0;
const SLOT_SEQUENCE: usize = 8;
const SLOT_GENERATION: usize = 16;
const SLOT_TIMESTAMP: usize = 24;
const SLOT_DROPPED: usize = 32;
const SLOT_PAYLOAD_LENGTH: usize = 40;
const SLOT_PLANE_COUNT: usize = 48;
const SLOT_PLANES: usize = 56;
const PLANE_BYTES: usize = 12;

pub struct BrokerMapping {
    mapping: Mapping,
    descriptor: MappingDescriptor,
    config: RingConfig,
    closed: bool,
    pinned: Option<FrameLease>,
}

impl BrokerMapping {
    pub fn create(config: RingConfig) -> HalResult<Self> {
        let identity = MappingIdentity::generate()?;
        let token = crate::MappingToken::generate()?;
        // Darwin limits POSIX shm names to 31 bytes. The 64-bit random name
        // remains collision-resistant because O_EXCL rejects any collision;
        // the independent 256-bit identity and capability remain full-length.
        let mut name_bytes = [0_u8; 8];
        fill(&mut name_bytes)
            .map_err(|error| internal("shared_memory.create", error.to_string()))?;
        let mut name = String::from("/seeed-hal-");
        for byte in name_bytes {
            write!(&mut name, "{byte:02x}")
                .expect("writing hexadecimal data to a String cannot fail");
        }
        let mapping = Mapping::create(&name, config.total_length())
            .map_err(|error| unavailable("shared_memory.create", error.to_string()))?;
        let descriptor = MappingDescriptor {
            name,
            identity: identity.clone(),
            token,
            total_length: config.total_length(),
        };
        let mut result = Self {
            mapping,
            descriptor,
            config,
            closed: false,
            pinned: None,
        };
        result.write_header(&identity)?;
        Ok(result)
    }

    pub fn descriptor(&self) -> &MappingDescriptor {
        &self.descriptor
    }

    pub fn writer(&mut self) -> SlotWriter<'_> {
        SlotWriter { broker: self }
    }

    pub fn validated_header(&self) -> HalResult<ValidatedHeader> {
        validate_header(
            self.mapping.as_ptr(),
            self.config.total_length(),
            &self.descriptor,
        )
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped_counter().load(Ordering::Acquire)
    }

    pub fn close(&mut self) -> HalResult<()> {
        if !self.closed {
            Mapping::unlink(&self.descriptor.name)
                .map_err(|error| unavailable("shared_memory.close", error.to_string()))?;
            self.closed = true;
        }
        Ok(())
    }

    /// Releases the preceding broker-owned pin and returns the newest committed
    /// frame lease. The control plane passes this lease to a read-only client.
    pub fn next_frame_lease(&mut self) -> HalResult<Option<FrameLease>> {
        self.release_pin()?;
        let mut latest: Option<(usize, u64)> = None;
        for index in 0..self.config.slot_count() {
            if self.slot_state(index).load(Ordering::Acquire) != SlotState::Ready.raw() {
                continue;
            }
            let sequence = self.slot_sequence(index).load(Ordering::Acquire);
            if latest.is_none_or(|(_, current)| sequence > current) {
                latest = Some((index, sequence));
            }
        }
        let Some((index, sequence)) = latest else {
            return Ok(None);
        };
        let state = self.slot_state(index);
        if state
            .compare_exchange(
                SlotState::Ready.raw(),
                SlotState::Pinned.raw(),
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return Ok(None);
        }
        let generation = self.slot_generation(index);
        if self.slot_sequence(index).load(Ordering::Acquire) != sequence {
            state.store(SlotState::Ready.raw(), Ordering::Release);
            return Ok(None);
        }
        let lease = FrameLease {
            slot_index: index,
            sequence,
            generation,
        };
        self.pinned = Some(lease);
        Ok(Some(lease))
    }

    pub fn release_pin(&mut self) -> HalResult<()> {
        if let Some(lease) = self.pinned.take() {
            if self.slot_sequence(lease.slot_index).load(Ordering::Acquire) == lease.sequence
                && self.slot_generation(lease.slot_index) == lease.generation
            {
                self.slot_state(lease.slot_index)
                    .store(SlotState::Free.raw(), Ordering::Release);
            }
        }
        Ok(())
    }

    fn write_header(&mut self, identity: &MappingIdentity) -> HalResult<()> {
        let base = self.mapping.as_ptr();
        // SAFETY: the broker owns a writable mapping exactly config.total_length bytes long.
        // All offsets below are fixed layout fields contained in HEADER_BYTES.
        unsafe {
            std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), base.add(HEADER_MAGIC), MAGIC.len());
            write_u16(base.add(HEADER_MAJOR), LAYOUT_MAJOR);
            write_u16(base.add(HEADER_MINOR), LAYOUT_MINOR);
            write_u64(
                base.add(HEADER_TOTAL_LENGTH),
                self.config.total_length() as u64,
            );
            write_u64(base.add(HEADER_SLOT_COUNT), self.config.slot_count() as u64);
            write_u64(
                base.add(HEADER_SLOT_STRIDE),
                self.config.slot_stride() as u64,
            );
            write_u32(
                base.add(HEADER_FORMAT),
                PixelFormat::from(self.config.format().pixel_format()) as u32,
            );
            write_u32(base.add(HEADER_WIDTH), self.config.format().width());
            write_u32(base.add(HEADER_HEIGHT), self.config.format().height());
            write_u64(
                base.add(HEADER_PAYLOAD_CAPACITY),
                self.config.payload_capacity() as u64,
            );
            std::ptr::copy_nonoverlapping(
                identity.bytes().as_ptr(),
                base.add(HEADER_IDENTITY),
                identity.bytes().len(),
            );
            std::ptr::copy_nonoverlapping(
                self.descriptor.token().hash().as_ptr(),
                base.add(HEADER_TOKEN_HASH),
                32,
            );
        }
        Ok(())
    }

    fn dropped_counter(&self) -> &AtomicU64 {
        // SAFETY: HEADER_BYTES is 192 bytes and HEADER_DROPPED_COUNT is 128, so
        // this fixed header field is 8-byte aligned and never overlaps slot zero.
        unsafe {
            &*(self
                .mapping
                .as_ptr()
                .add(HEADER_DROPPED_COUNT)
                .cast::<AtomicU64>())
        }
    }

    fn slot_base(&self, index: usize) -> *mut u8 {
        // SAFETY: all callers validate index against slot_count; config validation proves
        // header + index * stride stays within the owned mapping.
        unsafe {
            self.mapping
                .as_ptr()
                .add(HEADER_BYTES + index * self.config.slot_stride())
        }
    }

    fn select_writable_slot(&mut self) -> Option<usize> {
        let mut oldest: Option<(usize, u64)> = None;
        for index in 0..self.config.slot_count() {
            let state = self.slot_state(index).load(Ordering::Acquire);
            match SlotState::from_raw(state).ok()? {
                SlotState::Free => return Some(index),
                SlotState::Ready => {
                    let sequence = self.slot_sequence(index).load(Ordering::Acquire);
                    if oldest.is_none_or(|(_, oldest_sequence)| sequence < oldest_sequence) {
                        oldest = Some((index, sequence));
                    }
                }
                SlotState::Writing | SlotState::Pinned => {}
            }
        }
        oldest.map(|(index, _)| index)
    }

    fn slot_state(&self, index: usize) -> &AtomicU64 {
        // SAFETY: slot base is 64-byte aligned and SLOT_STATE is zero, satisfying atomic
        // alignment; field lies inside the slot header validated by RingConfig.
        unsafe { &*(self.slot_base(index).add(SLOT_STATE).cast::<AtomicU64>()) }
    }

    fn slot_sequence(&self, index: usize) -> &AtomicU64 {
        // SAFETY: slot base is aligned and SLOT_SEQUENCE is an 8-byte aligned fixed field.
        unsafe { &*(self.slot_base(index).add(SLOT_SEQUENCE).cast::<AtomicU64>()) }
    }

    fn slot_generation(&self, index: usize) -> u64 {
        // SAFETY: generation is a fixed aligned field initialized before Ready publication.
        unsafe { read_u64(self.slot_base(index).add(SLOT_GENERATION)) }
    }

    #[cfg(test)]
    pub(crate) fn corrupt_header_for_test(&mut self, offset: usize, byte: u8) {
        // SAFETY: tests use a header-bounded offset and broker mapping is writable.
        unsafe { *self.mapping.as_ptr().add(offset) = byte };
    }

    #[cfg(test)]
    pub(crate) fn torn_slot_for_test(&mut self, index: usize) {
        self.slot_state(index)
            .store(SlotState::Writing.raw(), Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn pin_all_slots_for_test(&mut self) {
        for index in 0..self.config.slot_count() {
            self.slot_state(index)
                .store(SlotState::Pinned.raw(), Ordering::Release);
        }
    }
}

impl Drop for BrokerMapping {
    fn drop(&mut self) {
        let _ = self.close();
    }
}

pub struct SlotWriter<'a> {
    broker: &'a mut BrokerMapping,
}

impl SlotWriter<'_> {
    pub fn publish(&mut self, metadata: FrameMetadata, payload: &[u8]) -> HalResult<()> {
        if payload.len() > self.broker.config.payload_capacity() {
            return Err(invalid(
                "shared_memory.publish",
                "payload exceeds the negotiated slot capacity",
            ));
        }
        validate_dimensions(metadata.format(), metadata.width(), metadata.height())?;
        validate_planes(
            metadata.format(),
            metadata.width(),
            metadata.height(),
            metadata.planes(),
            payload.len(),
        )?;
        let Some(index) = self.broker.select_writable_slot() else {
            self.broker.dropped_counter().fetch_add(1, Ordering::AcqRel);
            return Ok(());
        };
        if self.broker.slot_state(index).load(Ordering::Acquire) == SlotState::Ready.raw() {
            self.broker.dropped_counter().fetch_add(1, Ordering::AcqRel);
        }
        self.broker
            .slot_state(index)
            .store(SlotState::Writing.raw(), Ordering::Release);
        let base = self.broker.slot_base(index);
        // SAFETY: the broker owns the writable slot, marked Writing before these accesses.
        // RingConfig bounds header and payload storage, and metadata plane count is validated.
        unsafe {
            write_u64(base.add(SLOT_GENERATION), metadata.generation());
            write_u64(base.add(SLOT_TIMESTAMP), metadata.monotonic_timestamp_ns());
            write_u64(base.add(SLOT_DROPPED), self.broker.dropped_count());
            write_u64(base.add(SLOT_PAYLOAD_LENGTH), payload.len() as u64);
            write_u64(base.add(SLOT_PLANE_COUNT), metadata.planes().len() as u64);
            for (index, plane) in metadata.planes().iter().enumerate() {
                let offset = SLOT_PLANES + index * PLANE_BYTES;
                write_u32(base.add(offset), plane.offset() as u32);
                write_u32(base.add(offset + 4), plane.length() as u32);
                write_u32(base.add(offset + 8), plane.stride() as u32);
            }
            std::ptr::copy_nonoverlapping(
                payload.as_ptr(),
                base.add(SLOT_HEADER_BYTES),
                payload.len(),
            );
        }
        self.broker
            .slot_sequence(index)
            .store(metadata.sequence(), Ordering::Release);
        self.broker
            .slot_state(index)
            .store(SlotState::Ready.raw(), Ordering::Release);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrameLease {
    slot_index: usize,
    sequence: u64,
    generation: u64,
}

pub struct ReadOnlyMapping {
    mapping: Mapping,
    header: ValidatedHeader,
    expected_generation: Option<u64>,
}

impl ReadOnlyMapping {
    pub fn open(descriptor: &MappingDescriptor) -> HalResult<Self> {
        let mapping = Mapping::open_read_only(&descriptor.name, descriptor.total_length)
            .map_err(|error| unavailable("shared_memory.open", error.to_string()))?;
        let header = validate_header(mapping.as_ptr(), descriptor.total_length, descriptor)?;
        Ok(Self {
            mapping,
            header,
            expected_generation: None,
        })
    }

    pub fn read(&self, lease: FrameLease) -> HalResult<Option<FrameView<'_>>> {
        if lease.slot_index >= self.header.config().slot_count()
            || self.slot_state(lease.slot_index).load(Ordering::Acquire) != SlotState::Pinned.raw()
        {
            return Ok(None);
        }
        let generation = self.slot_generation(lease.slot_index)?;
        if self
            .expected_generation
            .is_some_and(|value| value != generation)
            || generation != lease.generation
            || self.slot_sequence(lease.slot_index).load(Ordering::Acquire) != lease.sequence
        {
            return Ok(None);
        }
        let metadata = self.read_metadata(lease.slot_index, lease.sequence, generation)?;
        if self.slot_sequence(lease.slot_index).load(Ordering::Acquire) != lease.sequence
            || self.slot_generation(lease.slot_index)? != generation
        {
            return Ok(None);
        }
        let base = self.slot_base(lease.slot_index);
        let payload_length = self.slot_payload_length(lease.slot_index)?;
        // SAFETY: payload_length was validated by read_metadata against the slot capacity and
        // the retained pin excludes producer overwrite for the returned self borrow.
        let payload =
            unsafe { std::slice::from_raw_parts(base.add(SLOT_HEADER_BYTES), payload_length) };
        Ok(Some(FrameView { metadata, payload }))
    }

    fn read_metadata(
        &self,
        index: usize,
        sequence: u64,
        generation: u64,
    ) -> HalResult<FrameMetadata> {
        let base = self.slot_base(index);
        // SAFETY: acquiring Ready state synchronizes with writer's release; each fixed metadata
        // field lies in the validated slot header. Payload slice is bounded below before creation.
        let (timestamp, dropped, payload_length, plane_count) = unsafe {
            (
                read_u64(base.add(SLOT_TIMESTAMP)),
                read_u64(base.add(SLOT_DROPPED)),
                read_u64(base.add(SLOT_PAYLOAD_LENGTH)) as usize,
                read_u64(base.add(SLOT_PLANE_COUNT)) as usize,
            )
        };
        if payload_length > self.header.config().payload_capacity() || plane_count > MAX_PLANES {
            return Err(invalid(
                "shared_memory.read",
                "slot payload or plane count exceeds the validated layout",
            ));
        }
        let mut planes = Vec::with_capacity(plane_count);
        for index in 0..plane_count {
            let offset = SLOT_PLANES + index * PLANE_BYTES;
            // SAFETY: plane_count is checked against fixed MAX_PLANES; all reads remain
            // within SLOT_HEADER_BYTES and fields are initialized before publication.
            let plane = unsafe {
                crate::PlaneLayout::new(
                    read_u32(base.add(offset)) as usize,
                    read_u32(base.add(offset + 4)) as usize,
                    read_u32(base.add(offset + 8)) as usize,
                )?
            };
            planes.push(plane);
        }
        let config = self.header.config();
        let format = PixelFormat::from(config.format().pixel_format());
        validate_planes(
            format,
            config.format().width(),
            config.format().height(),
            &planes,
            payload_length,
        )?;
        let metadata = FrameMetadata::new(
            format,
            config.format().width(),
            config.format().height(),
            sequence,
            generation,
            timestamp,
            dropped,
            planes,
        )?;
        Ok(metadata)
    }

    fn slot_base(&self, index: usize) -> *mut u8 {
        // SAFETY: index comes from slot_count-bounded loops; validated header proves range.
        unsafe {
            self.mapping
                .as_ptr()
                .add(HEADER_BYTES + index * self.header.config().slot_stride())
        }
    }

    fn slot_state(&self, index: usize) -> &AtomicU64 {
        // SAFETY: see BrokerMapping::slot_state alignment and fixed-layout evidence.
        unsafe { &*(self.slot_base(index).add(SLOT_STATE).cast::<AtomicU64>()) }
    }

    fn slot_sequence(&self, index: usize) -> &AtomicU64 {
        // SAFETY: see BrokerMapping::slot_sequence alignment and fixed-layout evidence.
        unsafe { &*(self.slot_base(index).add(SLOT_SEQUENCE).cast::<AtomicU64>()) }
    }

    fn slot_generation(&self, index: usize) -> HalResult<u64> {
        // SAFETY: generation is a fixed, aligned u64 header field initialized before release
        // publication and only read after an acquire state/sequence observation.
        Ok(unsafe { read_u64(self.slot_base(index).add(SLOT_GENERATION)) })
    }

    fn slot_payload_length(&self, index: usize) -> HalResult<usize> {
        // SAFETY: payload length is a fixed header field read after acquiring the pin state.
        let length = unsafe { read_u64(self.slot_base(index).add(SLOT_PAYLOAD_LENGTH)) as usize };
        if length > self.header.config().payload_capacity() {
            return Err(invalid(
                "shared_memory.read",
                "slot payload exceeds the validated capacity",
            ));
        }
        Ok(length)
    }

    #[cfg(test)]
    pub(crate) fn force_generation_mismatch_for_test(&mut self) {
        self.expected_generation = Some(u64::MAX);
    }
}

pub struct FrameView<'a> {
    metadata: FrameMetadata,
    payload: &'a [u8],
}

impl FrameView<'_> {
    pub fn metadata(&self) -> &FrameMetadata {
        &self.metadata
    }
    pub fn payload(&self) -> &[u8] {
        self.payload
    }
}

fn validate_header(
    base: *mut u8,
    mapped_length: usize,
    descriptor: &MappingDescriptor,
) -> HalResult<ValidatedHeader> {
    // SAFETY: caller supplies a successful mapping at least mapped_length bytes long; fixed
    // header reads only access HEADER_BYTES and are validated before derived offsets are used.
    let (
        magic,
        major,
        minor,
        total_length,
        slot_count,
        slot_stride,
        format,
        width,
        height,
        capacity,
    ) = unsafe {
        (
            std::slice::from_raw_parts(base.add(HEADER_MAGIC), MAGIC.len()),
            read_u16(base.add(HEADER_MAJOR)),
            read_u16(base.add(HEADER_MINOR)),
            read_u64(base.add(HEADER_TOTAL_LENGTH)) as usize,
            read_u64(base.add(HEADER_SLOT_COUNT)) as usize,
            read_u64(base.add(HEADER_SLOT_STRIDE)) as usize,
            read_u32(base.add(HEADER_FORMAT)),
            read_u32(base.add(HEADER_WIDTH)),
            read_u32(base.add(HEADER_HEIGHT)),
            read_u64(base.add(HEADER_PAYLOAD_CAPACITY)) as usize,
        )
    };
    if magic != MAGIC || major != LAYOUT_MAJOR || minor > LAYOUT_MINOR {
        return Err(invalid(
            "shared_memory.open",
            "mapping magic or layout version is incompatible",
        ));
    }
    if total_length != mapped_length || total_length != descriptor.total_length {
        return Err(invalid(
            "shared_memory.open",
            "mapping total length does not match its descriptor",
        ));
    }
    let format = PixelFormat::try_from(format)?;
    let camera_format = CameraFormat::new(
        match format {
            PixelFormat::Nv12 => seeed_hal_camera::CameraPixelFormat::Nv12,
            PixelFormat::Yuyv => seeed_hal_camera::CameraPixelFormat::Yuyv,
            PixelFormat::Mjpeg => seeed_hal_camera::CameraPixelFormat::Mjpeg,
        },
        width,
        height,
    )?;
    let config = RingConfig::new(camera_format, slot_count, capacity)?;
    if config.slot_stride() != slot_stride {
        return Err(invalid(
            "shared_memory.open",
            "mapping slot stride violates the fixed layout",
        ));
    }
    // SAFETY: fixed 32-byte values lie within HEADER_BYTES; comparison never exposes secrets.
    let identity = unsafe { std::slice::from_raw_parts(base.add(HEADER_IDENTITY), 32) };
    // SAFETY: fixed 32-byte values lie within HEADER_BYTES; comparison never exposes secrets.
    let token_hash = unsafe { std::slice::from_raw_parts(base.add(HEADER_TOKEN_HASH), 32) };
    if identity != descriptor.identity.bytes().as_slice()
        || !bool::from(subtle::ConstantTimeEq::ct_eq(
            token_hash,
            descriptor.token().hash().as_slice(),
        ))
    {
        return Err(invalid(
            "shared_memory.open",
            "mapping session identity or capability token does not match",
        ));
    }
    Ok(ValidatedHeader::new(config, descriptor.identity.clone()))
}

// SAFETY: callers provide a valid mapping address with room for the fixed-width field.
unsafe fn write_u16(pointer: *mut u8, value: u16) {
    // SAFETY: inherited from this helper's caller.
    unsafe { std::ptr::write_unaligned(pointer.cast(), value.to_le()) };
}
// SAFETY: callers provide a valid mapping address with room for the fixed-width field.
unsafe fn write_u32(pointer: *mut u8, value: u32) {
    // SAFETY: inherited from this helper's caller.
    unsafe { std::ptr::write_unaligned(pointer.cast(), value.to_le()) };
}
// SAFETY: callers provide a valid mapping address with room for the fixed-width field.
unsafe fn write_u64(pointer: *mut u8, value: u64) {
    // SAFETY: inherited from this helper's caller.
    unsafe { std::ptr::write_unaligned(pointer.cast(), value.to_le()) };
}
// SAFETY: callers provide a valid mapping address with room for the fixed-width field.
unsafe fn read_u16(pointer: *const u8) -> u16 {
    // SAFETY: inherited from this helper's caller.
    unsafe { u16::from_le(std::ptr::read_unaligned(pointer.cast())) }
}
// SAFETY: callers provide a valid mapping address with room for the fixed-width field.
unsafe fn read_u32(pointer: *const u8) -> u32 {
    // SAFETY: inherited from this helper's caller.
    unsafe { u32::from_le(std::ptr::read_unaligned(pointer.cast())) }
}
// SAFETY: callers provide a valid mapping address with room for the fixed-width field.
unsafe fn read_u64(pointer: *const u8) -> u64 {
    // SAFETY: inherited from this helper's caller.
    unsafe { u64::from_le(std::ptr::read_unaligned(pointer.cast())) }
}
