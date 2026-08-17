#![deny(unsafe_op_in_unsafe_fn)]

mod layout;
mod platform;
mod ring;

pub use layout::{
    FrameMetadata, MAX_MAPPING_BYTES, MappingDescriptor, MappingIdentity, MappingToken,
    PixelFormat, PlaneLayout, RingConfig, SlotState, ValidatedHeader,
};
pub use ring::{BrokerMapping, CopiedFrame, FrameLease, FrameView, ReadOnlyMapping, SlotWriter};

use seeed_hal_core::{ErrorCategory, HalError, HalResult};

pub(crate) fn invalid(operation: &'static str, message: &'static str) -> HalError {
    HalError::new(
        "shared_memory.invalid",
        ErrorCategory::InvalidArgument,
        operation,
        false,
        message,
    )
    .expect("static shared-memory error metadata is valid")
}

pub(crate) fn unavailable(operation: &'static str, message: impl Into<String>) -> HalError {
    HalError::new(
        "shared_memory.unavailable",
        ErrorCategory::Unavailable,
        operation,
        false,
        message,
    )
    .expect("static shared-memory error metadata is valid")
}

pub(crate) fn internal(operation: &'static str, message: String) -> HalError {
    HalError::new(
        "shared_memory.internal",
        ErrorCategory::Internal,
        operation,
        false,
        message,
    )
    .expect("static shared-memory error metadata is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use seeed_hal_camera::{CameraFormat, CameraPixelFormat};

    fn config() -> RingConfig {
        RingConfig::new(
            CameraFormat::new(CameraPixelFormat::Yuyv, 2, 2).unwrap(),
            4,
            64,
        )
        .unwrap()
    }

    fn metadata(sequence: u64, generation: u64) -> FrameMetadata {
        FrameMetadata::new(
            PixelFormat::Yuyv,
            2,
            2,
            sequence,
            generation,
            123,
            0,
            vec![PlaneLayout::new(0, 8, 4).unwrap()],
        )
        .unwrap()
    }

    #[test]
    fn acquires_a_broker_pinned_zero_copy_frame() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        broker.writer().publish(metadata(1, 1), &[1; 8]).unwrap();

        let frame = broker.acquire().unwrap().unwrap();
        assert_eq!(frame.payload(), &[1; 8]);
        assert_eq!(frame.metadata().sequence(), 1);
    }

    #[test]
    fn independently_reopened_mapping_only_returns_an_owned_copy() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        let descriptor = broker.descriptor().clone();
        broker.writer().publish(metadata(1, 1), &[1; 8]).unwrap();

        let mut client = ReadOnlyMapping::open(&descriptor).unwrap();
        let lease = broker.next_frame_lease().unwrap().unwrap();
        let frame = client.copy(lease).unwrap().unwrap();
        assert_eq!(frame.payload(), &[1; 8]);
        assert_eq!(frame.metadata().sequence(), 1);
        broker.release_pin().unwrap();
    }

    #[test]
    fn an_open_reader_rejects_a_frame_lease_after_the_broker_closes() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        let descriptor = broker.descriptor().clone();
        broker.writer().publish(metadata(1, 1), &[1; 8]).unwrap();

        let mut client = ReadOnlyMapping::open(&descriptor).unwrap();
        let lease = broker.next_frame_lease().unwrap().unwrap();
        broker.close().unwrap();

        assert!(client.copy(lease).unwrap().is_none());
    }

    #[test]
    fn rejects_bad_capability_token() {
        let broker = BrokerMapping::create(config()).unwrap();
        let mut descriptor = broker.descriptor().clone();
        descriptor.replace_token_for_test(MappingToken::generate().unwrap());

        assert!(ReadOnlyMapping::open(&descriptor).is_err());
    }

    #[test]
    fn rejects_malformed_header_and_unaligned_slot_stride() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        broker.corrupt_header_for_test(0, 0);
        assert!(broker.validated_header().is_err());

        let error = RingConfig::new(
            CameraFormat::new(CameraPixelFormat::Yuyv, 2, 2).unwrap(),
            4,
            7,
        )
        .unwrap_err();
        assert_eq!(error.category(), ErrorCategory::InvalidArgument);
    }

    #[test]
    fn rejects_plane_escaping_payload_and_generation_mismatch() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        let escaping = FrameMetadata::new(
            PixelFormat::Yuyv,
            2,
            2,
            1,
            1,
            1,
            0,
            vec![PlaneLayout::new(4, 8, 4).unwrap()],
        )
        .unwrap();
        assert!(broker.writer().publish(escaping, &[1; 8]).is_err());

        broker.writer().publish(metadata(2, 5), &[2; 8]).unwrap();
        let mut client = ReadOnlyMapping::open(broker.descriptor()).unwrap();
        client.force_generation_mismatch_for_test();
        let lease = broker.next_frame_lease().unwrap().unwrap();
        assert!(client.copy(lease).unwrap().is_none());
    }

    #[test]
    fn detects_torn_publication() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        broker.writer().publish(metadata(1, 1), &[1; 8]).unwrap();
        broker.torn_slot_for_test(0);
        assert!(broker.next_frame_lease().unwrap().is_none());
    }

    #[test]
    fn latest_wins_releases_previous_pin_and_counts_drops_when_all_slots_are_pinned() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        for sequence in 1..=4 {
            broker
                .writer()
                .publish(metadata(sequence, sequence), &[sequence as u8; 8])
                .unwrap();
        }

        let frame = broker.acquire().unwrap().unwrap();
        assert_eq!(frame.metadata().sequence(), 4);
        drop(frame);
        broker.writer().publish(metadata(5, 5), &[5; 8]).unwrap();
        let frame = broker.acquire().unwrap().unwrap();
        assert_eq!(frame.metadata().sequence(), 5);
        drop(frame);

        broker.pin_all_slots_for_test();
        assert!(broker.writer().publish(metadata(6, 6), &[6; 8]).is_ok());
        assert_eq!(broker.dropped_count(), 2);
    }

    #[test]
    fn validates_mapping_total_length_overflow() {
        assert!(
            RingConfig::new(
                CameraFormat::new(CameraPixelFormat::Mjpeg, 4096, 2160).unwrap(),
                8,
                usize::MAX,
            )
            .is_err()
        );
    }
}
