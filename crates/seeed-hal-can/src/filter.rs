use seeed_hal_core::{ErrorCategory, HalError, HalResult};

use crate::{CanFrame, MAX_CAN_FILTERS};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanIdFormat {
    Standard,
    Extended,
    Either,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CanFrameClasses {
    data: bool,
    remote: bool,
    error: bool,
}

impl CanFrameClasses {
    pub const fn new(data: bool, remote: bool, error: bool) -> Self {
        Self {
            data,
            remote,
            error,
        }
    }

    pub const fn data_only() -> Self {
        Self::new(true, false, false)
    }

    pub fn data(&self) -> bool {
        self.data
    }

    pub fn remote(&self) -> bool {
        self.remote
    }

    pub fn error(&self) -> bool {
        self.error
    }

    fn any(self) -> bool {
        self.data || self.remote || self.error
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanFilter {
    id: u32,
    mask: u32,
    format: CanIdFormat,
    classes: CanFrameClasses,
}

impl CanFilter {
    pub fn new(
        id: u32,
        mask: u32,
        format: CanIdFormat,
        classes: CanFrameClasses,
    ) -> HalResult<Self> {
        let max = if matches!(format, CanIdFormat::Standard) {
            0x7ff
        } else {
            0x1fff_ffff
        };
        if id > max || mask > max {
            return Err(invalid_filter("CAN filter ID or mask exceeds format width"));
        }
        if !classes.any() {
            return Err(invalid_filter("CAN filter must enable a frame class"));
        }
        Ok(Self {
            id,
            mask,
            format,
            classes,
        })
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn mask(&self) -> u32 {
        self.mask
    }

    pub fn format(&self) -> CanIdFormat {
        self.format
    }

    pub fn classes(&self) -> CanFrameClasses {
        self.classes
    }

    pub fn matches(&self, frame: &CanFrame) -> bool {
        if frame.is_error() {
            return self.classes.error();
        }
        let (id, is_standard, class_enabled) = match frame {
            CanFrame::ClassicData { id, .. } | CanFrame::FdData { id, .. } => {
                (id.value(), id.is_standard(), self.classes.data())
            }
            CanFrame::ClassicRemote { id, .. } => {
                (id.value(), id.is_standard(), self.classes.remote())
            }
            CanFrame::Error { .. } => unreachable!(),
        };
        if !class_enabled {
            return false;
        }
        let format_matches = match self.format {
            CanIdFormat::Standard => is_standard,
            CanIdFormat::Extended => !is_standard,
            CanIdFormat::Either => true,
        };
        format_matches && (id & self.mask) == (self.id & self.mask)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanFilterSet(Vec<CanFilter>);

impl CanFilterSet {
    pub fn new(filters: Vec<CanFilter>) -> HalResult<Self> {
        if filters.len() > MAX_CAN_FILTERS {
            return Err(invalid_filter("CAN filter set exceeds 64 filters"));
        }
        Ok(Self(filters))
    }

    pub fn as_slice(&self) -> &[CanFilter] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn matches(&self, frame: &CanFrame) -> bool {
        self.0.is_empty() || self.0.iter().any(|filter| filter.matches(frame))
    }
}

fn invalid_filter(message: &'static str) -> HalError {
    HalError::new(
        "can.filter.invalid",
        ErrorCategory::InvalidArgument,
        "can.filter",
        false,
        message,
    )
    .expect("static CAN filter error metadata is valid")
}
