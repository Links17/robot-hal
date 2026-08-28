#![deny(unsafe_op_in_unsafe_fn)]

mod layout;
mod platform;
mod ring;

pub use layout::{
    FrameMetadata, MAX_MAPPING_BYTES, MappingDescriptor, MappingIdentity, MappingToken,
    PixelFormat, PlaneLayout, RingConfig, SlotState, ValidatedHeader,
};
pub use ring::{BrokerMapping, CopiedFrame, FrameLease, FrameView, ReadOnlyMapping, SlotWriter};

use robot_hal_core::{ErrorCategory, HalError, HalResult};

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
    use robot_hal_camera::{CameraFormat, CameraPixelFormat};
    use std::process::Command;

    const CLOSE_READER_NAME: &str = "ROBOT_HAL_CLOSE_READER_NAME";
    const CLOSE_READER_LENGTH: &str = "ROBOT_HAL_CLOSE_READER_LENGTH";
    const CLOSE_READER_READY: &str = "ROBOT_HAL_CLOSE_READER_READY";
    #[cfg(windows)]
    const ABANDONED_CLOSE_NAME: &str = "ROBOT_HAL_ABANDONED_CLOSE_NAME";
    #[cfg(windows)]
    const ABANDONED_CLOSE_LENGTH: &str = "ROBOT_HAL_ABANDONED_CLOSE_LENGTH";

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
    fn producer_copies_directly_into_one_ring_slot() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        let mut copies = 0;
        broker
            .writer()
            .publish_with(metadata(1, 1), &mut |destination| {
                copies += 1;
                destination[..8].copy_from_slice(&[7; 8]);
                Ok(8)
            })
            .unwrap();

        let frame = broker.acquire().unwrap().unwrap();
        assert_eq!(copies, 1);
        assert_eq!(frame.payload(), &[7; 8]);
    }

    #[test]
    fn producer_rejects_a_payload_length_larger_than_the_destination() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        let error = broker
            .writer()
            .publish_with(metadata(1, 1), &mut |destination| Ok(destination.len() + 1))
            .expect_err("a producer may not claim bytes beyond its provided slot");

        assert_eq!(error.name().as_str(), "shared_memory.invalid");
    }

    #[test]
    fn failed_direct_copy_restores_the_slot_for_a_later_frame() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        for sequence in 1..=4 {
            let error = broker
                .writer()
                .publish_with(metadata(sequence, 1), &mut |destination| {
                    destination[..4].copy_from_slice(&[9; 4]);
                    Err(invalid(
                        "shared_memory.publish",
                        "simulated native-copy failure",
                    ))
                })
                .expect_err("native copier failure must be reported");
            assert_eq!(error.name().as_str(), "shared_memory.invalid");
        }

        broker.writer().publish(metadata(5, 1), &[7; 8]).unwrap();
        let frame = broker.acquire().unwrap().unwrap();
        assert_eq!(frame.metadata().sequence(), 5);
        assert_eq!(frame.payload(), &[7; 8]);
    }

    #[test]
    fn oversized_direct_copy_result_restores_the_slot_for_a_later_frame() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        for sequence in 1..=4 {
            let error = broker
                .writer()
                .publish_with(metadata(sequence, 1), &mut |destination| {
                    destination[..8].copy_from_slice(&[9; 8]);
                    Ok(destination.len() + 1)
                })
                .expect_err("native copier may not report more bytes than its slot");
            assert_eq!(error.name().as_str(), "shared_memory.invalid");
        }

        broker.writer().publish(metadata(5, 1), &[7; 8]).unwrap();
        let frame = broker.acquire().unwrap().unwrap();
        assert_eq!(frame.metadata().sequence(), 5);
        assert_eq!(frame.payload(), &[7; 8]);
    }

    #[test]
    fn independently_reopened_mapping_only_returns_an_owned_copy() {
        let mut broker = BrokerMapping::create(config()).unwrap();
        let descriptor = broker.descriptor().clone();
        broker.writer().publish(metadata(1, 1), &[1; 8]).unwrap();

        let mut client = ReadOnlyMapping::open(&descriptor).unwrap();
        assert_eq!(client.slot_count(), 4);
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

    #[cfg(windows)]
    #[test]
    fn abandoned_reader_lock_allows_terminal_close_but_no_frame_recovery() {
        if let (Ok(name), Ok(length)) = (
            std::env::var(ABANDONED_CLOSE_NAME),
            std::env::var(ABANDONED_CLOSE_LENGTH),
        ) {
            let mapping =
                crate::platform::Mapping::open_read_only(&name, length.parse().unwrap()).unwrap();
            mapping.try_lock_shared().unwrap();
            // SAFETY: this isolated child intentionally exits while owning the Windows mutex to
            // simulate a crashed reader. It must not run Rust destructors that would unlock it.
            unsafe { windows_sys::Win32::System::Threading::ExitProcess(0) };
        }

        let mut broker = BrokerMapping::create(config()).unwrap();
        let descriptor = broker.descriptor().clone();
        broker.writer().publish(metadata(1, 1), &[1; 8]).unwrap();
        let lease = broker.next_frame_lease().unwrap().unwrap();
        let mut reader = ReadOnlyMapping::open(&descriptor).unwrap();

        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("tests::abandoned_reader_lock_allows_terminal_close_but_no_frame_recovery")
            .arg("--nocapture")
            .env(ABANDONED_CLOSE_NAME, &descriptor.name)
            .env(ABANDONED_CLOSE_LENGTH, descriptor.total_length.to_string())
            .status()
            .unwrap();
        assert!(status.success());

        broker
            .close()
            .expect("terminal close must handle an abandoned reader mutex");
        assert!(
            reader.copy(lease).unwrap().is_none(),
            "close must only publish terminal state, never recover frame data"
        );
    }

    #[test]
    fn close_waits_for_a_reader_then_unlinks_the_mapping() {
        if let (Ok(name), Ok(length), Ok(ready)) = (
            std::env::var(CLOSE_READER_NAME),
            std::env::var(CLOSE_READER_LENGTH),
            std::env::var(CLOSE_READER_READY),
        ) {
            let mapping =
                crate::platform::Mapping::open_read_only(&name, length.parse().unwrap()).unwrap();
            mapping.try_lock_shared().unwrap();
            std::fs::write(ready, []).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            return;
        }

        let mut broker = BrokerMapping::create(config()).unwrap();
        let descriptor = broker.descriptor().clone();
        broker.writer().publish(metadata(1, 1), &[1; 8]).unwrap();
        let lease = broker.next_frame_lease().unwrap().unwrap();

        let mut reader = ReadOnlyMapping::open(&descriptor).unwrap();
        let ready =
            std::env::temp_dir().join(format!("robot-hal-close-reader-{}", std::process::id()));
        let _ = std::fs::remove_file(&ready);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("tests::close_waits_for_a_reader_then_unlinks_the_mapping")
            .arg("--nocapture")
            .env(CLOSE_READER_NAME, &descriptor.name)
            .env(CLOSE_READER_LENGTH, descriptor.total_length.to_string())
            .env(CLOSE_READER_READY, &ready)
            .spawn()
            .unwrap();
        while !ready.exists() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        broker.close().unwrap();
        assert!(child.wait().unwrap().success());
        std::fs::remove_file(ready).unwrap();
        assert!(reader.copy(lease).unwrap().is_none());
        assert!(ReadOnlyMapping::open(&descriptor).is_err());
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
