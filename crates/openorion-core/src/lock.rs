//! One Session per user: `flock` on `openorion_ipc::lock_path()` (DESIGN.md 6.9).

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::Error;

/// Take the exclusive lock. The lock lives as long as the returned file.
pub(crate) fn acquire(path: &Path) -> Result<File, Error> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|source| Error::Lock {
            path: path.to_owned(),
            source,
        })?;
    match f.try_lock() {
        Ok(()) => Ok(f),
        Err(TryLockError::WouldBlock) => Err(Error::Locked(path.to_owned())),
        Err(TryLockError::Error(source)) => Err(Error::Lock {
            path: path.to_owned(),
            source,
        }),
    }
}
