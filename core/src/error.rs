//! The single error type used across the crate.

use std::fmt;

/// Every fallible function in this crate returns this error.
///
/// Network and parse failures are always reported through this enum; the library
/// never panics on them. `Display` strings are short and suitable for a toast.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Local I/O failure (file, socket, process spawn).
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// HTTP transport failure or an unexpected HTTP status.
    #[error("HTTP error: {0}")]
    Http(String),
    /// The TV answered a UPnP/SOAP action with a fault (e.g. "Transport is locked").
    #[error("TV refused {action}: {detail}")]
    Soap {
        /// SOAP action name, e.g. `Play`.
        action: String,
        /// `errorDescription` from the fault, or the HTTP status if absent.
        detail: String,
    },
    /// WebSocket (remote-control channel) failure.
    #[error("WebSocket error: {0}")]
    WebSocket(String),
    /// The remote-control channel is not connected.
    #[error("not connected to the TV remote channel")]
    NotConnected,
    /// The TV rejected our pairing request / token.
    #[error("the TV denied access (approve the pairing prompt on the TV)")]
    Unauthorized,
    /// JSON (de)serialisation failure.
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    /// Malformed data from the TV, ffprobe, or a user-supplied string.
    #[error("parse error: {0}")]
    Parse(String),
    /// An operation did not complete in time.
    #[error("timed out: {0}")]
    Timeout(String),
    /// Config file problem.
    #[error("config error: {0}")]
    Config(String),
    /// ffmpeg / ffprobe failure.
    #[error("media tool error: {0}")]
    Media(String),
    /// The requested feature or capability is not available.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// SmartThings cloud API failure (including missing token).
    #[error("SmartThings: {0}")]
    SmartThings(String),
    /// The operation was cancelled by the caller.
    #[error("cancelled")]
    Cancelled,
}

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Helper to build an [`Error::Parse`] from anything displayable.
    pub fn parse(msg: impl fmt::Display) -> Self {
        Error::Parse(msg.to_string())
    }
}

impl From<ureq::Error> for Error {
    fn from(e: ureq::Error) -> Self {
        match e {
            ureq::Error::Status(code, resp) => {
                let url = resp.get_url().to_string();
                Error::Http(format!("HTTP {code} from {url}"))
            }
            ureq::Error::Transport(t) => Error::Http(t.to_string()),
        }
    }
}

impl From<tungstenite::Error> for Error {
    fn from(e: tungstenite::Error) -> Self {
        Error::WebSocket(e.to_string())
    }
}
