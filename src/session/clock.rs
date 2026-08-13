//! Clock boundary used to make event timestamps deterministic in tests.

use std::time::{SystemTime, UNIX_EPOCH};

use super::{UnixMillis, error::ClockError};

/// Supplies Unix-epoch milliseconds to session construction and live appends.
pub trait Clock: Send + Sync {
    /// Return the current time as a JavaScript-safe integer millisecond value.
    fn now(&self) -> Result<UnixMillis, ClockError>;
}

/// Production clock backed by [`SystemTime`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Result<UnixMillis, ClockError> {
        let duration = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| {
                ClockError::new(format!("system clock is before the Unix epoch: {error}"))
            })?;
        let millis = i64::try_from(duration.as_millis())
            .map_err(|_| ClockError::new("system time does not fit in signed milliseconds"))?;
        UnixMillis::new(millis).map_err(|error| ClockError::new(error.to_string()))
    }
}
