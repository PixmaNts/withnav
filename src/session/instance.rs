#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(super) use unix::{stale_lock, Instance};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(super) use windows::{stale_lock, Instance};
