//! Session: the only place that sequences requests (DESIGN.md sec. 6). WP6.
//!
//! `Session::start` takes any `Transport` (hidraw, simulator, test fake),
//! checks that cyclic reports flow (resetting once if not), takes the
//! per-user lock and runs a full sync. The returned `SessionHandle` is the
//! API for the daemon and `openorion --direct`: semantic `Op`s in, `Event`s out.
//!
//! - `build`: `Op` → full set messages from the cached state (pure).
//! - `restore`: `plan_restore`, the minimal ops from one state to another.
//! - `session`: the task that owns the transport, the request queue,
//!   coalescing, verify, status/meter streams and health.

mod build;
mod error;
mod io;
mod lock;
mod restore;
mod session;

pub use build::{build_sets, set_path};
pub use error::Error;
pub use restore::{afx_power_up_target, plan_restore};
pub use session::{Event, HealthInfo, Session, SessionHandle, SessionOpts, Timings};
