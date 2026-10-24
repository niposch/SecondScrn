//! Session engines and policies shared by every SecondScrn platform.
//!
//! * [`host::HostEngine`] runs on the PC, [`client::ClientEngine`] on the
//!   tablet. Both are platform-independent and talk to OS-specific code only
//!   through the traits in [`platform`].
//! * [`negotiate`], [`adapt`] and [`backoff`] are pure, deterministic policy
//!   modules with unit tests.
//! * [`settings`] and [`snapshot`] define what the native UIs read and write.

pub mod adapt;
pub mod backoff;
pub mod channel;
pub mod client;
pub mod host;
pub mod negotiate;
pub mod platform;
pub mod settings;
pub mod snapshot;
#[cfg(feature = "test-support")]
pub mod test_support;

use secondscrn_protocol::RejectReason;
use secondscrn_protocol::framing::FrameError;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error("timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("the connection was closed")]
    ConnectionClosed,
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error("rejected: {0}")]
    Rejected(RejectReason),
    #[error("the host's identity does not match the paired PC")]
    HostIdentityChanged,
    #[error("pairing failed: the code did not match")]
    PairingFailed,
    #[error("security: {0}")]
    Security(#[from] secondscrn_security::SecurityError),
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("control channel: {0}")]
    Frame(#[from] FrameError),
    #[error("media transport: {0}")]
    Transport(String),
    #[error("{0}")]
    Platform(#[from] platform::PlatformError),
    #[error("invalid {0}")]
    InvalidInput(&'static str),
    #[error("{0} not found")]
    NotFound(&'static str),
}

impl From<secondscrn_transport::TransportError> for SessionError {
    fn from(e: secondscrn_transport::TransportError) -> Self {
        SessionError::Transport(e.to_string())
    }
}

pub use client::{ClientConfig, ClientEngine, ConnectTarget};
pub use host::{HostConfig, HostEngine};
