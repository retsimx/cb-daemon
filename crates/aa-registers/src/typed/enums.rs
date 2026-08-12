//! Wire field enums for typed register payloads.

/// Power / system on-off (reg `05` byte 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Power {
    /// System off (`0x00`).
    Off,
    /// System on (`0x01`).
    On,
    /// Unrecognised wire value (round-trips unchanged).
    Unknown(u8),
}

impl Power {
    /// Map a raw byte to a typed variant.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0x00 => Self::Off,
            0x01 => Self::On,
            other => Self::Unknown(other),
        }
    }

    /// Raw wire byte.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Off => 0x00,
            Self::On => 0x01,
            Self::Unknown(v) => v,
        }
    }
}

impl From<u8> for Power {
    fn from(value: u8) -> Self {
        Self::from_u8(value)
    }
}

impl From<Power> for u8 {
    fn from(value: Power) -> Self {
        value.to_u8()
    }
}

/// HVAC operating mode (reg `05` byte 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Cool (`0x01`).
    Cool,
    /// Heat (`0x02`).
    Heat,
    /// Vent (`0x03`).
    Vent,
    /// Auto (`0x04`).
    Auto,
    /// Dry (`0x05`).
    Dry,
    /// `MyAuto` (`0x06`).
    MyAuto,
    /// Unrecognised wire value (round-trips unchanged).
    Unknown(u8),
}

impl Mode {
    /// Map a raw byte to a typed variant.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0x01 => Self::Cool,
            0x02 => Self::Heat,
            0x03 => Self::Vent,
            0x04 => Self::Auto,
            0x05 => Self::Dry,
            0x06 => Self::MyAuto,
            other => Self::Unknown(other),
        }
    }

    /// Raw wire byte.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Cool => 0x01,
            Self::Heat => 0x02,
            Self::Vent => 0x03,
            Self::Auto => 0x04,
            Self::Dry => 0x05,
            Self::MyAuto => 0x06,
            Self::Unknown(v) => v,
        }
    }
}

impl From<u8> for Mode {
    fn from(value: u8) -> Self {
        Self::from_u8(value)
    }
}

impl From<Mode> for u8 {
    fn from(value: Mode) -> Self {
        value.to_u8()
    }
}

/// Fan speed (reg `05` byte 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Fan {
    /// Off (`0x00`).
    Off,
    /// Low (`0x01`).
    Low,
    /// Medium (`0x02`).
    Medium,
    /// High (`0x03`).
    High,
    /// Auto (`0x04`).
    Auto,
    /// `AutoAA` (`0x05`).
    AutoAa,
    /// Unrecognised wire value (round-trips unchanged).
    Unknown(u8),
}

impl Fan {
    /// Map a raw byte to a typed variant.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0x00 => Self::Off,
            0x01 => Self::Low,
            0x02 => Self::Medium,
            0x03 => Self::High,
            0x04 => Self::Auto,
            0x05 => Self::AutoAa,
            other => Self::Unknown(other),
        }
    }

    /// Raw wire byte.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Off => 0x00,
            Self::Low => 0x01,
            Self::Medium => 0x02,
            Self::High => 0x03,
            Self::Auto => 0x04,
            Self::AutoAa => 0x05,
            Self::Unknown(v) => v,
        }
    }
}

impl From<u8> for Fan {
    fn from(value: u8) -> Self {
        Self::from_u8(value)
    }
}

impl From<Fan> for u8 {
    fn from(value: Fan) -> Self {
        value.to_u8()
    }
}

/// Fresh-air damper status (reg `05` byte 5).
///
/// README table: `00` = none, `01` = off, `02` = on (not `monitor_aa.py`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FreshAir {
    /// No fresh-air hardware / not applicable (`0x00`).
    None,
    /// Fresh air off (`0x01`).
    Off,
    /// Fresh air on (`0x02`).
    On,
    /// Unrecognised wire value (round-trips unchanged).
    Unknown(u8),
}

impl FreshAir {
    /// Map a raw byte to a typed variant.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0x00 => Self::None,
            0x01 => Self::Off,
            0x02 => Self::On,
            other => Self::Unknown(other),
        }
    }

    /// Raw wire byte.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::None => 0x00,
            Self::Off => 0x01,
            Self::On => 0x02,
            Self::Unknown(v) => v,
        }
    }
}

impl From<u8> for FreshAir {
    fn from(value: u8) -> Self {
        Self::from_u8(value)
    }
}

impl From<FreshAir> for u8 {
    fn from(value: FreshAir) -> Self {
        value.to_u8()
    }
}

/// Zone temperature sensor type (reg `03` byte 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SensorType {
    /// No sensor (`0x00`).
    NoSensor,
    /// RF sensor (`0x01`).
    Rf,
    /// Wired sensor (`0x02`).
    Wired,
    /// RF2CAN booster (`0x03`).
    Rf2CanBooster,
    /// `RF_X` (`0x04`).
    RfX,
    /// Unrecognised wire value (round-trips unchanged).
    Unknown(u8),
}

impl SensorType {
    /// Map a raw byte to a typed variant.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0x00 => Self::NoSensor,
            0x01 => Self::Rf,
            0x02 => Self::Wired,
            0x03 => Self::Rf2CanBooster,
            0x04 => Self::RfX,
            other => Self::Unknown(other),
        }
    }

    /// Raw wire byte.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::NoSensor => 0x00,
            Self::Rf => 0x01,
            Self::Wired => 0x02,
            Self::Rf2CanBooster => 0x03,
            Self::RfX => 0x04,
            Self::Unknown(v) => v,
        }
    }
}

impl From<u8> for SensorType {
    fn from(value: u8) -> Self {
        Self::from_u8(value)
    }
}

impl From<SensorType> for u8 {
    fn from(value: SensorType) -> Self {
        value.to_u8()
    }
}

/// Aircon unit brand (reg `02` byte 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnitBrand {
    /// Daikin (`0x11`).
    Daikin,
    /// Panasonic (`0x12`).
    Panasonic,
    /// Fujitsu (`0x13`).
    Fujitsu,
    /// Samsung DVM (`0x19`).
    SamsungDvm,
    /// Unrecognised wire value (round-trips unchanged).
    Unknown(u8),
}

impl UnitBrand {
    /// Map a raw byte to a typed variant.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0x11 => Self::Daikin,
            0x12 => Self::Panasonic,
            0x13 => Self::Fujitsu,
            0x19 => Self::SamsungDvm,
            other => Self::Unknown(other),
        }
    }

    /// Raw wire byte.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Daikin => 0x11,
            Self::Panasonic => 0x12,
            Self::Fujitsu => 0x13,
            Self::SamsungDvm => 0x19,
            Self::Unknown(v) => v,
        }
    }
}

impl From<u8> for UnitBrand {
    fn from(value: u8) -> Self {
        Self::from_u8(value)
    }
}

impl From<UnitBrand> for u8 {
    fn from(value: UnitBrand) -> Self {
        value.to_u8()
    }
}

/// Activation-code status (reg `02` byte 1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ActivationStatus {
    /// No code (`0x00`).
    NoCode,
    /// Code enabled (`0x01`).
    CodeEnabled,
    /// Expired (`0x02`).
    Expired,
    /// Unrecognised wire value (round-trips unchanged).
    Unknown(u8),
}

impl ActivationStatus {
    /// Map a raw byte to a typed variant.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0x00 => Self::NoCode,
            0x01 => Self::CodeEnabled,
            0x02 => Self::Expired,
            other => Self::Unknown(other),
        }
    }

    /// Raw wire byte.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::NoCode => 0x00,
            Self::CodeEnabled => 0x01,
            Self::Expired => 0x02,
            Self::Unknown(v) => v,
        }
    }
}

impl From<u8> for ActivationStatus {
    fn from(value: u8) -> Self {
        Self::from_u8(value)
    }
}

impl From<ActivationStatus> for u8 {
    fn from(value: ActivationStatus) -> Self {
        value.to_u8()
    }
}

/// Activation-code action (reg `09` byte 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    /// Set code (`0x01`).
    SetCode,
    /// Unlock (`0x02`).
    Unlock,
    /// Unrecognised wire value (round-trips unchanged).
    Unknown(u8),
}

impl Action {
    /// Map a raw byte to a typed variant.
    #[must_use]
    pub const fn from_u8(value: u8) -> Self {
        match value {
            0x01 => Self::SetCode,
            0x02 => Self::Unlock,
            other => Self::Unknown(other),
        }
    }

    /// Raw wire byte.
    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::SetCode => 0x01,
            Self::Unlock => 0x02,
            Self::Unknown(v) => v,
        }
    }
}

impl From<u8> for Action {
    fn from(value: u8) -> Self {
        Self::from_u8(value)
    }
}

impl From<Action> for u8 {
    fn from(value: Action) -> Self {
        value.to_u8()
    }
}
