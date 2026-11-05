use std::path::PathBuf;

use openorion_proto::ProtoError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    /// Another Session (openoriond or `openorion --direct`) holds the lock.
    #[error("another openorion session is running (lock {0} is held); use the daemon or stop it")]
    Locked(PathBuf),
    #[error("cannot take lock {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// No cyclic reports, even after a reset.
    #[error("the Orion sends no cyclic reports, even after a USB reset; power-cycle it")]
    NoCyclic,
    /// A USB reset was needed or asked for, but it would cut the running
    /// USB audio stream.
    #[error(
        "the Orion's USB audio is in use ({0}); a USB reset would interrupt it. \
         Stop the audio, or force the reset"
    )]
    ResetBlocked(String),
    /// Requests go unanswered while cyclic reports still flow.
    #[error("the Orion's control channel hangs (requests unanswered); power-cycle it")]
    Hung,
    /// Cyclic reports stopped; settings are refused until they return.
    #[error(
        "the Orion's cyclic reports stopped; settings are refused until they return \
         (wait, or run `openorion reset`, which interrupts USB audio)"
    )]
    Stalled,
    #[error("transport error: {0}")]
    Io(String),
    #[error("the device went away")]
    Disconnected,
    /// The cache cannot build the full set yet (no sync, or the part is
    /// missing from the cache).
    #[error("state not synced yet: {0}")]
    NotSynced(String),
    #[error("invalid operation: {0}")]
    Invalid(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Proto(#[from] ProtoError),
    /// The session task has stopped.
    #[error("session closed")]
    Closed,
}
