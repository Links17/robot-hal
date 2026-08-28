use robot_hal_core::{ErrorCategory, HalError, HalResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CanMode {
    Classic,
    Fd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CanBitTiming {
    bitrate: u32,
    sample_point_permill: Option<u16>,
    sjw: Option<u16>,
}

impl CanBitTiming {
    pub fn new(
        bitrate: u32,
        sample_point_permill: Option<u16>,
        sjw: Option<u16>,
    ) -> HalResult<Self> {
        if bitrate == 0 {
            return Err(invalid_config("CAN bitrate must be nonzero"));
        }
        if sample_point_permill.is_some_and(|value| !(1..=999).contains(&value)) {
            return Err(invalid_config("CAN sample point must be 1..=999 permill"));
        }
        if sjw.is_some_and(|value| value == 0) {
            return Err(invalid_config("CAN SJW must be nonzero"));
        }
        Ok(Self {
            bitrate,
            sample_point_permill,
            sjw,
        })
    }

    pub fn bitrate(&self) -> u32 {
        self.bitrate
    }

    pub fn sample_point_permill(&self) -> Option<u16> {
        self.sample_point_permill
    }

    pub fn sjw(&self) -> Option<u16> {
        self.sjw
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanLinkExpectation {
    mode: Option<CanMode>,
    nominal_bitrate: Option<u32>,
    data_bitrate: Option<u32>,
    listen_only: Option<bool>,
    loopback: Option<bool>,
}

impl CanLinkExpectation {
    pub fn new(
        mode: Option<CanMode>,
        nominal_bitrate: Option<u32>,
        data_bitrate: Option<u32>,
        listen_only: Option<bool>,
        loopback: Option<bool>,
    ) -> HalResult<Self> {
        if nominal_bitrate.is_some_and(|value| value == 0)
            || data_bitrate.is_some_and(|value| value == 0)
        {
            return Err(invalid_config("CAN expected bitrates must be nonzero"));
        }
        if mode == Some(CanMode::Classic) && data_bitrate.is_some() {
            return Err(invalid_config(
                "Classical CAN expectation cannot include data bitrate",
            ));
        }
        Ok(Self {
            mode,
            nominal_bitrate,
            data_bitrate,
            listen_only,
            loopback,
        })
    }

    pub fn mode(&self) -> Option<CanMode> {
        self.mode
    }

    pub fn nominal_bitrate(&self) -> Option<u32> {
        self.nominal_bitrate
    }

    pub fn data_bitrate(&self) -> Option<u32> {
        self.data_bitrate
    }

    pub fn listen_only(&self) -> Option<bool> {
        self.listen_only
    }

    pub fn loopback(&self) -> Option<bool> {
        self.loopback
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanConfigureConfig {
    mode: CanMode,
    nominal: CanBitTiming,
    data: Option<CanBitTiming>,
    listen_only: bool,
    loopback: bool,
    restart_ms: Option<u32>,
}

impl CanConfigureConfig {
    pub fn new(
        mode: CanMode,
        nominal: CanBitTiming,
        data: Option<CanBitTiming>,
        listen_only: bool,
        loopback: bool,
    ) -> HalResult<Self> {
        Self::new_with_restart(mode, nominal, data, listen_only, loopback, None)
    }

    pub fn new_with_restart(
        mode: CanMode,
        nominal: CanBitTiming,
        data: Option<CanBitTiming>,
        listen_only: bool,
        loopback: bool,
        restart_ms: Option<u32>,
    ) -> HalResult<Self> {
        if restart_ms.is_some_and(|value| value == 0) {
            return Err(invalid_config(
                "CAN restart time must be nonzero when specified",
            ));
        }
        match (mode, data.is_some()) {
            (CanMode::Classic, true) => {
                return Err(invalid_config(
                    "Classical CAN configuration cannot include data timing",
                ));
            }
            (CanMode::Fd, false) => {
                return Err(invalid_config("CAN FD configuration requires data timing"));
            }
            _ => {}
        }
        Ok(Self {
            mode,
            nominal,
            data,
            listen_only,
            loopback,
            restart_ms,
        })
    }

    pub fn mode(&self) -> CanMode {
        self.mode
    }

    pub fn nominal(&self) -> &CanBitTiming {
        &self.nominal
    }

    pub fn data(&self) -> Option<&CanBitTiming> {
        self.data.as_ref()
    }

    pub fn listen_only(&self) -> bool {
        self.listen_only
    }

    pub fn loopback(&self) -> bool {
        self.loopback
    }

    pub fn restart_ms(&self) -> Option<u32> {
        self.restart_ms
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CanOpenConfig {
    Attach(CanLinkExpectation),
    Configure(CanConfigureConfig),
}

fn invalid_config(message: &'static str) -> HalError {
    HalError::new(
        "can.configuration.invalid",
        ErrorCategory::InvalidArgument,
        "can.configuration",
        false,
        message,
    )
    .expect("static CAN configuration error metadata is valid")
}
