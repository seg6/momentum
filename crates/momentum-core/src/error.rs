use thiserror::Error;

/// Errors raised by the protocol layer.
#[derive(Debug, Error)]
pub enum Error {
    #[error("parameter {name} out of range: {value} not in {min}..={max}")]
    ParameterRange {
        name: &'static str,
        value: i64,
        min: i64,
        max: i64,
    },

    #[error("response payload too short for {command}: got {got}B, expected >= {expected}B")]
    PayloadTooShort {
        command: &'static str,
        got: usize,
        expected: usize,
    },

    #[error("GAIA packet too short")]
    ShortPacket,

    #[error("SPP frame too large ({0} bytes)")]
    FrameTooLarge(usize),

    #[error("missing SPP frame marker")]
    MissingFrameMarker,

    #[error("short SPP frame")]
    ShortFrame,

    #[error("unsupported SPP frame version {0}")]
    UnsupportedFrameVersion(u8),

    #[error("unsupported SPP frame flags {0:#04x}")]
    UnsupportedFrameFlags(u8),

    #[error("incomplete SPP frame")]
    IncompleteFrame,

    #[error("GAIA error response for {command} ({response:#06x})")]
    GaiaError {
        command: &'static str,
        response: u16,
        payload: Box<[u8]>,
    },

    #[error("unexpected GAIA response {got:#06x} for {command}; expected {expected:#06x}")]
    UnexpectedResponse {
        command: &'static str,
        got: u16,
        expected: u16,
    },

    #[error("unexpected GAIA vendor {got:#06x} for {command}; expected {expected:#06x}")]
    UnexpectedVendor {
        command: &'static str,
        got: u16,
        expected: u16,
    },

    #[error("transport error")]
    Transport(#[source] Box<dyn std::error::Error + Send + Sync + 'static>),
}

pub type Result<T> = core::result::Result<T, Error>;
