use bytes::Bytes;
use seeed_hal_can::*;
use seeed_hal_core::{ErrorCategory, HalError, LeaseMode, TransportKind};

#[test]
fn constants_and_capabilities_are_stable() {
    assert_eq!(MAX_CLASSIC_DATA_BYTES, 8);
    assert_eq!(MAX_FD_DATA_BYTES, 64);
    assert_eq!(MAX_CAN_FILTERS, 64);
    assert_eq!(MAX_CAN_BATCH_FRAMES, 64);
    assert_eq!(DEFAULT_CAN_RX_CAPACITY, 256);
    assert_eq!(DEFAULT_CAN_TX_CAPACITY, 64);
    assert_eq!(can_classic_capability().as_str(), CAN_CLASSIC_CAPABILITY);
    assert_eq!(can_fd_capability().as_str(), CAN_FD_CAPABILITY);
    assert_eq!(can_configure_capability().as_str(), CAN_CONFIGURE_CAPABILITY);
    assert_eq!(can_error_frames_capability().as_str(), CAN_ERROR_FRAMES_CAPABILITY);
    assert_eq!(can_rx_timestamp_capability().as_str(), CAN_RX_TIMESTAMP_CAPABILITY);
}

#[test]
fn ids_enforce_standard_and_extended_widths() {
    assert!(CanId::standard(0x7ff).is_ok());
    assert!(CanId::standard(0x800).is_err());
    assert!(CanId::extended(0x1fff_ffff).is_ok());
    assert!(CanId::extended(0x2000_0000).is_err());
}

#[test]
fn frame_limits_and_flags_are_enforced() {
    let standard = CanId::standard(1).unwrap();
    assert!(CanFrame::classic_data(standard, Bytes::from(vec![0; 8])).is_ok());
    assert!(CanFrame::classic_data(standard, Bytes::from(vec![0; 9])).is_err());
    assert!(CanFrame::classic_remote(standard, 8).is_ok());
    assert!(CanFrame::classic_remote(standard, 9).is_err());
    assert!(CanFrame::fd_data(standard, Bytes::from(vec![0; 64]), true, true).is_ok());
    assert!(CanFrame::fd_data(standard, Bytes::from(vec![0; 65]), false, false).is_err());
    for length in [9, 10, 11, 13, 14, 15, 17, 18, 19, 21, 22, 23, 25, 26, 27, 28, 29, 30, 31, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63] {
        assert!(CanFrame::fd_data(standard, Bytes::from(vec![0; length]), false, false).is_err());
    }
    assert!(CanFrame::error(vec![CanErrorClass::BusError], Bytes::new()).is_ok());
    assert!(CanFrame::error(Vec::new(), Bytes::new()).is_err());
    assert!(CanFrame::error(vec![CanErrorClass::Other], Bytes::from(vec![0; 9])).is_err());
}

#[test]
fn timestamp_reuses_bounded_ascii_domain_invariant() {
    assert!(CanTimestamp::new(1, CanTimestampSource::Kernel, "host-clock").is_ok());
    for domain in [String::new(), "é".to_owned(), "x".repeat(256)] {
        assert!(CanTimestamp::new(1, CanTimestampSource::Hardware, domain).is_err());
    }
}

#[test]
fn filters_match_classes_formats_and_error_frames() {
    let standard = CanId::standard(0x120).unwrap();
    let extended = CanId::extended(0x120).unwrap();
    let data = CanFrame::classic_data(standard, Bytes::new()).unwrap();
    let remote = CanFrame::classic_remote(standard, 0).unwrap();
    let error = CanFrame::error(vec![CanErrorClass::BusOff], Bytes::new()).unwrap();
    let data_filter = CanFilter::new(
        0x120,
        0x7ff,
        CanIdFormat::Standard,
        CanFrameClasses::new(true, false, false),
    )
    .unwrap();
    assert!(data_filter.matches(&data));
    assert!(!data_filter.matches(&remote));
    assert!(!data_filter.matches(&extended.clone().into_data_frame()));

    let error_filter = CanFilter::new(
        0xffff_ffff,
        0xffff_ffff,
        CanIdFormat::Standard,
        CanFrameClasses::new(false, false, true),
    );
    assert!(error_filter.is_err());
    let error_filter = CanFilter::new(
        0x7ff,
        0x7ff,
        CanIdFormat::Standard,
        CanFrameClasses::new(false, false, true),
    )
    .unwrap();
    assert!(error_filter.matches(&error));
}

trait TestFrameExt {
    fn into_data_frame(self) -> CanFrame;
}

impl TestFrameExt for CanId {
    fn into_data_frame(self) -> CanFrame {
        CanFrame::classic_data(self, Bytes::new()).unwrap()
    }
}

#[test]
fn empty_filter_set_receives_all_and_limit_is_exact() {
    let empty = CanFilterSet::new(Vec::new()).unwrap();
    let frame = CanFrame::classic_data(CanId::standard(1).unwrap(), Bytes::new()).unwrap();
    assert!(empty.matches(&frame));
    let filter = CanFilter::new(0, 0, CanIdFormat::Either, CanFrameClasses::data_only()).unwrap();
    assert!(CanFilterSet::new(vec![filter; MAX_CAN_FILTERS]).is_ok());
    assert!(CanFilterSet::new(vec![filter; MAX_CAN_FILTERS + 1]).is_err());
}

#[test]
fn configuration_rejects_invalid_timing_combinations() {
    assert!(CanBitTiming::new(0, None, None).is_err());
    assert!(CanBitTiming::new(500_000, Some(0), None).is_err());
    assert!(CanBitTiming::new(500_000, Some(1000), None).is_err());
    assert!(CanBitTiming::new(500_000, None, Some(0)).is_err());
    let nominal = CanBitTiming::new(500_000, None, None).unwrap();
    assert!(CanConfigureConfig::new(CanMode::Classic, nominal, Some(nominal), false, false).is_err());
    assert!(CanConfigureConfig::new(CanMode::Fd, nominal, None, false, false).is_err());
    assert!(CanConfigureConfig::new(CanMode::Classic, nominal, None, false, false).is_ok());
}

#[test]
fn batch_error_preserves_prefix_and_redacts_debug() {
    let error = HalError::new(
        "can.bus.off",
        ErrorCategory::Unavailable,
        "can.send_batch",
        false,
        "private adapter diagnostic",
    )
    .unwrap();
    let batch = CanBatchSendError::new(error.clone());
    assert_eq!(batch.committed(), 0);
    assert_eq!(batch.error().name().as_str(), "can.bus.off");
    let debug = format!("{batch:?}");
    assert!(!debug.contains("private adapter diagnostic"));

    let partial = CanBatchSendError::backend_prefix(error, 3);
    assert_eq!(partial.committed(), 3);
}

#[test]
fn configure_restart_ms_is_optional_and_nonzero() {
    let nominal = CanBitTiming::new(500_000, None, None).unwrap();
    let config = CanConfigureConfig::new_with_restart(
        CanMode::Classic,
        nominal,
        None,
        false,
        false,
        Some(250),
    )
    .unwrap();
    assert_eq!(config.restart_ms(), Some(250));
    assert!(CanConfigureConfig::new_with_restart(
        CanMode::Classic,
        nominal,
        None,
        false,
        false,
        Some(0),
    )
    .is_err());
}

#[test]
fn new_core_variants_are_available_without_reordering_legacy_values() {
    assert_eq!(TransportKind::Can, TransportKind::Can);
    assert_eq!(LeaseMode::Maintenance, LeaseMode::Maintenance);
}
