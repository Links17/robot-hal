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
const HEADER_TERMINAL_STATE: usize = 136;
const TERMINAL_STATE_OPEN: u64 = 0;
const TERMINAL_STATE_CLOSED: u64 = 1;

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
        // Darwin limits POSIX shm names to 30 usable bytes. The OS object name is not an
        // authority: it is paired with a distinct 256-bit identity and capability token. The
        // platform limit prevents a 256-bit name, so O_EXCL plus 72 random bits handles naming
        // collisions; all authorization remains on the independent 256-bit capability.
        let mut name_bytes = [0_u8; 9];
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
        if let Err(error) = result.write_header(&identity) {
            let unlink_result = Mapping::unlink(&result.descriptor.name);
            result.closed = unlink_result.is_ok();
            return Err(error);
        }
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
        self.mapping
            .try_lock_shared()
            .ok()
            .map(|()| {
                // SAFETY: a shared OS lock excludes the writer's exclusive lock. The fixed
                // counter field is within the mapped header and is copied as raw bytes.
                let value = unsafe { read_u64(self.mapping.as_ptr().add(HEADER_DROPPED_COUNT)) };
                let _ = self.mapping.unlock();
                value
            })
            .unwrap_or(0)
    }

    pub fn close(&mut self) -> HalResult<()> {
        if !self.closed {
            loop {
                match self.mapping.try_lock_exclusive() {
                    Ok(()) => break,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    Err(error) => {
                        return Err(unavailable("shared_memory.close", error.to_string()));
                    }
                }
            }
            // SAFETY: the broker holds the exclusive mapping lock and the fixed terminal-state
            // field lies within the writable header.
            unsafe {
                write_u64(
                    self.mapping.as_ptr().add(HEADER_TERMINAL_STATE),
                    TERMINAL_STATE_CLOSED,
                )
            };
            self.mapping
                .unlock()
                .map_err(|error| unavailable("shared_memory.close", error.to_string()))?;
            Mapping::unlink(&self.descriptor.name)
                .map_err(|error| unavailable("shared_memory.close", error.to_string()))?;
            self.closed = true;
        }
        Ok(())
    }

    /// Releases the preceding broker-owned pin and returns the newest committed
    /// frame lease. The control plane passes this lease to a read-only client.
    pub fn next_frame_lease(&mut self) -> HalResult<Option<FrameLease>> {
        self.mapping
            .try_lock_exclusive()
            .map_err(|error| unavailable("shared_memory.lease", error.to_string()))?;
        let result = self.next_frame_lease_locked();
        self.mapping
            .unlock()
            .map_err(|error| unavailable("shared_memory.lease", error.to_string()))?;
        result
    }

    fn next_frame_lease_locked(&mut self) -> HalResult<Option<FrameLease>> {
        self.release_pin_locked()?;
        let mut latest: Option<(usize, u64)> = None;
        for index in 0..self.config.slot_count() {
            if self.slot_state(index)? != SlotState::Ready {
                continue;
            }
            let sequence = self.slot_sequence(index)?;
            if latest.is_none_or(|(_, current)| sequence > current) {
                latest = Some((index, sequence));
            }
        }
        let Some((index, sequence)) = latest else {
            return Ok(None);
        };
        if self.slot_state(index)? != SlotState::Ready {
            return Ok(None);
        }
        self.write_slot_state(index, SlotState::Pinned)?;
        let generation = self.slot_generation(index);
        if self.slot_sequence(index)? != sequence {
            self.write_slot_state(index, SlotState::Ready)?;
            return Ok(None);
        }
        let lease = FrameLease {
            identity: self.descriptor.identity.clone(),
            slot_index: index,
            sequence,
            generation,
        };
        self.pinned = Some(lease.clone());
        Ok(Some(lease))
    }

    pub fn release_pin(&mut self) -> HalResult<()> {
        self.mapping
            .try_lock_exclusive()
            .map_err(|error| unavailable("shared_memory.release", error.to_string()))?;
        let result = self.release_pin_locked();
        self.mapping
            .unlock()
            .map_err(|error| unavailable("shared_memory.release", error.to_string()))?;
        result
    }

    fn release_pin_locked(&mut self) -> HalResult<()> {
        if let Some(lease) = self.pinned.take() {
            if lease.identity == self.descriptor.identity
                && self.slot_sequence(lease.slot_index)? == lease.sequence
                && self.slot_generation(lease.slot_index) == lease.generation
                && self.slot_state(lease.slot_index)? == SlotState::Pinned
            {
                self.write_slot_state(lease.slot_index, SlotState::Free)?;
            }
        }
        Ok(())
    }

    /// Returns a zero-copy frame while retaining the broker-owned pin and a shared OS lock.
    /// The exclusive `&mut self` borrow prevents `writer`, `acquire`, `release_pin`, or any
    /// subsequent lease operation until the `FrameView` is dropped.
    pub fn acquire(&mut self) -> HalResult<Option<FrameView<'_>>> {
        self.mapping
            .try_lock_exclusive()
            .map_err(|error| unavailable("shared_memory.acquire", error.to_string()))?;
        let lease = match self.next_frame_lease_locked() {
            Ok(lease) => lease,
            Err(error) => {
                let _ = self.mapping.unlock();
                return Err(error);
            }
        };
        let Some(lease) = lease else {
            self.mapping
                .unlock()
                .map_err(|error| unavailable("shared_memory.acquire", error.to_string()))?;
            return Ok(None);
        };
        self.mapping
            .unlock()
            .map_err(|error| unavailable("shared_memory.acquire", error.to_string()))?;
        self.mapping
            .try_lock_shared()
            .map_err(|error| unavailable("shared_memory.acquire", error.to_string()))?;
        let frame = self.frame_view(lease);
        if frame.is_err() {
            let _ = self.mapping.unlock();
        }
        frame.map(Some)
    }

    fn frame_view(&self, lease: FrameLease) -> HalResult<FrameView<'_>> {
        if lease.identity != self.descriptor.identity
            || self.slot_state(lease.slot_index)? != SlotState::Pinned
            || self.slot_sequence(lease.slot_index)? != lease.sequence
            || self.slot_generation(lease.slot_index) != lease.generation
        {
            return Err(invalid(
                "shared_memory.acquire",
                "pinned lease no longer matches slot",
            ));
        }
        let metadata = read_metadata(
            &self.mapping,
            &self.config,
            lease.slot_index,
            lease.sequence,
            lease.generation,
        )?;
        let payload_length = read_payload_length(&self.mapping, &self.config, lease.slot_index)?;
        let base = self.slot_base(lease.slot_index);
        // SAFETY: the retained broker pin prevents writer selection and FrameView retains the
        // shared OS lock until Drop. The range was validated by read_metadata.
        let payload =
            unsafe { std::slice::from_raw_parts(base.add(SLOT_HEADER_BYTES), payload_length) };
        Ok(FrameView {
            metadata,
            payload,
            mapping: &self.mapping,
        })
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
            write_u64(base.add(HEADER_TERMINAL_STATE), TERMINAL_STATE_OPEN);
        }
        Ok(())
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

    fn select_writable_slot(&mut self) -> HalResult<Option<usize>> {
        let mut oldest: Option<(usize, u64)> = None;
        for index in 0..self.config.slot_count() {
            match self.slot_state(index)? {
                SlotState::Free => return Ok(Some(index)),
                SlotState::Ready => {
                    let sequence = self.slot_sequence(index)?;
                    if oldest.is_none_or(|(_, oldest_sequence)| sequence < oldest_sequence) {
                        oldest = Some((index, sequence));
                    }
                }
                SlotState::Writing | SlotState::Pinned => {}
            }
        }
        Ok(oldest.map(|(index, _)| index))
    }

    fn slot_state(&self, index: usize) -> HalResult<SlotState> {
        // SAFETY: callers hold either the writer's exclusive OS lock or a reader's shared OS
        // lock; fixed state field lies within the validated slot header.
        SlotState::from_raw(unsafe { read_u64(self.slot_base(index).add(SLOT_STATE)) })
    }

    fn write_slot_state(&mut self, index: usize, state: SlotState) -> HalResult<()> {
        // SAFETY: the broker is the only slot-state writer and callers hold its exclusive
        // OS lock. The fixed field is inside the owned writable mapping.
        unsafe { write_u64(self.slot_base(index).add(SLOT_STATE), state.raw()) };
        Ok(())
    }

    fn dropped_count_locked(&self) -> u64 {
        // SAFETY: caller holds the broker's exclusive OS lock; fixed counter field is valid.
        unsafe { read_u64(self.mapping.as_ptr().add(HEADER_DROPPED_COUNT)) }
    }

    fn increment_dropped_locked(&mut self) -> HalResult<()> {
        let next = self
            .dropped_count_locked()
            .checked_add(1)
            .ok_or_else(|| invalid("shared_memory.publish", "dropped-frame counter overflow"))?;
        // SAFETY: caller holds the broker's exclusive OS lock; fixed counter field is valid.
        unsafe { write_u64(self.mapping.as_ptr().add(HEADER_DROPPED_COUNT), next) };
        Ok(())
    }

    fn increment_dropped_unlocked(&mut self) -> HalResult<()> {
        // A producer never blocks: a contended lock counts as an immediately dropped frame only
        // when it can subsequently obtain the lock without waiting.
        if self.mapping.try_lock_exclusive().is_ok() {
            let result = self.increment_dropped_locked();
            let unlock = self.mapping.unlock();
            result?;
            unlock.map_err(|error| unavailable("shared_memory.publish", error.to_string()))?;
        }
        Ok(())
    }

    fn slot_sequence(&self, index: usize) -> HalResult<u64> {
        // SAFETY: callers hold an OS lock excluding concurrent writers; field is fixed-layout.
        Ok(unsafe { read_u64(self.slot_base(index).add(SLOT_SEQUENCE)) })
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
        self.write_slot_state(index, SlotState::Writing).unwrap();
    }

    #[cfg(test)]
    pub(crate) fn pin_all_slots_for_test(&mut self) {
        for index in 0..self.config.slot_count() {
            self.write_slot_state(index, SlotState::Pinned).unwrap();
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
        match self.broker.mapping.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                self.broker.increment_dropped_unlocked()?;
                return Ok(());
            }
            Err(error) => return Err(unavailable("shared_memory.publish", error.to_string())),
        }
        let result = self.publish_locked(metadata, payload);
        self.broker
            .mapping
            .unlock()
            .map_err(|error| unavailable("shared_memory.publish", error.to_string()))?;
        result
    }

    fn publish_locked(&mut self, metadata: FrameMetadata, payload: &[u8]) -> HalResult<()> {
        let Some(index) = self.broker.select_writable_slot()? else {
            self.broker.increment_dropped_locked()?;
            return Ok(());
        };
        if self.broker.slot_state(index)? == SlotState::Ready {
            self.broker.increment_dropped_locked()?;
        }
        self.broker.write_slot_state(index, SlotState::Writing)?;
        let base = self.broker.slot_base(index);
        // SAFETY: the broker owns the writable slot, marked Writing before these accesses.
        // RingConfig bounds header and payload storage, and metadata plane count is validated.
        unsafe {
            write_u64(base.add(SLOT_GENERATION), metadata.generation());
            write_u64(base.add(SLOT_TIMESTAMP), metadata.monotonic_timestamp_ns());
            write_u64(base.add(SLOT_DROPPED), self.broker.dropped_count_locked());
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
        // SAFETY: exclusive OS lock excludes every reader; sequence is written before Ready.
        unsafe {
            write_u64(
                self.broker.slot_base(index).add(SLOT_SEQUENCE),
                metadata.sequence(),
            )
        };
        self.broker.write_slot_state(index, SlotState::Ready)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameLease {
    identity: MappingIdentity,
    slot_index: usize,
    sequence: u64,
    generation: u64,
}

impl FrameLease {
    pub fn slot_index(&self) -> usize {
        self.slot_index
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn with_identity(
        identity: MappingIdentity,
        slot_index: usize,
        sequence: u64,
        generation: u64,
    ) -> Self {
        Self {
            identity,
            slot_index,
            sequence,
            generation,
        }
    }
}

/// Copy-only frame returned by a separately reopened mapping. It owns its payload, so it cannot
/// escape the lease/pin lifetime as a zero-copy reference (the required Python-facing boundary).
pub struct CopiedFrame {
    metadata: FrameMetadata,
    payload: Vec<u8>,
}

impl CopiedFrame {
    pub fn metadata(&self) -> &FrameMetadata {
        &self.metadata
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
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

    /// Copies a pinned frame under a shared OS lock. This API intentionally never yields a
    /// mapping-backed byte slice; external language bindings must use this operation.
    pub fn copy(&mut self, lease: FrameLease) -> HalResult<Option<CopiedFrame>> {
        if lease.identity != *self.header.identity() {
            return Ok(None);
        }
        self.mapping
            .try_lock_shared()
            .map_err(|error| unavailable("shared_memory.copy", error.to_string()))?;
        let result = self.copy_locked(lease);
        self.mapping
            .unlock()
            .map_err(|error| unavailable("shared_memory.copy", error.to_string()))?;
        result
    }

    fn copy_locked(&self, lease: FrameLease) -> HalResult<Option<CopiedFrame>> {
        if self.terminal_state()? != TERMINAL_STATE_OPEN {
            return Ok(None);
        }
        if lease.slot_index >= self.header.config().slot_count()
            || self.slot_state(lease.slot_index)? != SlotState::Pinned
        {
            return Ok(None);
        }
        let generation = self.slot_generation(lease.slot_index)?;
        if self
            .expected_generation
            .is_some_and(|value| value != generation)
            || generation != lease.generation
            || self.slot_sequence(lease.slot_index)? != lease.sequence
        {
            return Ok(None);
        }
        let metadata = self.read_metadata(lease.slot_index, lease.sequence, generation)?;
        if self.slot_sequence(lease.slot_index)? != lease.sequence
            || self.slot_generation(lease.slot_index)? != generation
        {
            return Ok(None);
        }
        let base = self.slot_base(lease.slot_index);
        let payload_length = self.slot_payload_length(lease.slot_index)?;
        // SAFETY: payload_length was validated against slot capacity and the shared OS lock plus
        // broker-held pin exclude producer overwrite for this copy operation.
        let payload =
            unsafe { std::slice::from_raw_parts(base.add(SLOT_HEADER_BYTES), payload_length) };
        Ok(Some(CopiedFrame {
            metadata,
            payload: payload.to_vec(),
        }))
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

    fn slot_state(&self, index: usize) -> HalResult<SlotState> {
        // SAFETY: caller holds the shared OS lock; field is within validated slot header.
        SlotState::from_raw(unsafe { read_u64(self.slot_base(index).add(SLOT_STATE)) })
    }

    fn terminal_state(&self) -> HalResult<u64> {
        // SAFETY: caller holds the shared mapping lock and the fixed terminal-state field is
        // within the validated header.
        Ok(unsafe { read_u64(self.mapping.as_ptr().add(HEADER_TERMINAL_STATE)) })
    }

    fn slot_sequence(&self, index: usize) -> HalResult<u64> {
        // SAFETY: caller holds the shared OS lock; field is within validated slot header.
        Ok(unsafe { read_u64(self.slot_base(index).add(SLOT_SEQUENCE)) })
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

fn slot_base(mapping: &Mapping, config: &RingConfig, index: usize) -> *mut u8 {
    // SAFETY: callers ensure index is bounded by the validated slot count; config arithmetic
    // establishes that the fixed slot range lies within this mapping.
    unsafe {
        mapping
            .as_ptr()
            .add(HEADER_BYTES + index * config.slot_stride())
    }
}

fn read_payload_length(mapping: &Mapping, config: &RingConfig, index: usize) -> HalResult<usize> {
    // SAFETY: caller holds an OS lock; payload length is a fixed header field in this slot.
    let length =
        unsafe { read_u64(slot_base(mapping, config, index).add(SLOT_PAYLOAD_LENGTH)) as usize };
    if length > config.payload_capacity() {
        return Err(invalid(
            "shared_memory.read",
            "slot payload exceeds the validated capacity",
        ));
    }
    Ok(length)
}

fn read_metadata(
    mapping: &Mapping,
    config: &RingConfig,
    index: usize,
    sequence: u64,
    generation: u64,
) -> HalResult<FrameMetadata> {
    let base = slot_base(mapping, config, index);
    // SAFETY: caller holds an OS lock, so fixed fields cannot race a writer. They lie within
    // SLOT_HEADER_BYTES and the mapping layout was validated before the caller reached here.
    let (timestamp, dropped, payload_length, plane_count) = unsafe {
        (
            read_u64(base.add(SLOT_TIMESTAMP)),
            read_u64(base.add(SLOT_DROPPED)),
            read_u64(base.add(SLOT_PAYLOAD_LENGTH)) as usize,
            read_u64(base.add(SLOT_PLANE_COUNT)) as usize,
        )
    };
    if payload_length > config.payload_capacity() || plane_count > MAX_PLANES {
        return Err(invalid(
            "shared_memory.read",
            "slot payload or plane count exceeds the validated layout",
        ));
    }
    let mut planes = Vec::with_capacity(plane_count);
    for index in 0..plane_count {
        let offset = SLOT_PLANES + index * PLANE_BYTES;
        // SAFETY: plane_count is bounded by MAX_PLANES, so all plane fields are in the header.
        let plane = unsafe {
            crate::PlaneLayout::new(
                read_u32(base.add(offset)) as usize,
                read_u32(base.add(offset + 4)) as usize,
                read_u32(base.add(offset + 8)) as usize,
            )?
        };
        planes.push(plane);
    }
    let format = PixelFormat::from(config.format().pixel_format());
    validate_planes(
        format,
        config.format().width(),
        config.format().height(),
        &planes,
        payload_length,
    )?;
    FrameMetadata::new(
        format,
        config.format().width(),
        config.format().height(),
        sequence,
        generation,
        timestamp,
        dropped,
        planes,
    )
}

pub struct FrameView<'a> {
    metadata: FrameMetadata,
    payload: &'a [u8],
    mapping: &'a Mapping,
}

impl FrameView<'_> {
    pub fn metadata(&self) -> &FrameMetadata {
        &self.metadata
    }
    pub fn payload(&self) -> &[u8] {
        self.payload
    }
}

impl Drop for FrameView<'_> {
    fn drop(&mut self) {
        // A failing unlock cannot be safely reported from Drop. The public close/release APIs
        // remain fallible; an unlock failure leaves the OS lock to process teardown rather than
        // falsely claiming cleanup succeeded.
        let _ = self.mapping.unlock();
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
    if total_length != descriptor.total_length || mapped_length < total_length {
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
    let header_token_hash = unsafe { std::slice::from_raw_parts(base.add(HEADER_TOKEN_HASH), 32) };
    let identity_matches = subtle::ConstantTimeEq::ct_eq(identity, descriptor.identity.bytes());
    let token_hash = descriptor.token().hash();
    let token_matches = subtle::ConstantTimeEq::ct_eq(header_token_hash, token_hash.as_slice());
    if !bool::from(identity_matches & token_matches) {
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
