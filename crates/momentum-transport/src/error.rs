use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Protocol(#[from] momentum_core::Error),

    #[error("I/O error")]
    Io(#[from] std::io::Error),

    #[error("invalid Bluetooth address: {0}")]
    InvalidAddress(String),

    #[error("SPP connection closed by peer")]
    Closed,

    #[error("Bluetooth transport is not implemented on {}", std::env::consts::OS)]
    Unsupported,
}

pub type Result<T> = core::result::Result<T, Error>;
