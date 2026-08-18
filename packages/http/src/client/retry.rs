use std::{num::NonZeroU32, time::Duration};

use super::{BuildError, BuildErrorKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    pub(super) max_attempts: NonZeroU32,
    initial_backoff: Duration,
    max_backoff: Duration,
}

impl Retry {
    pub fn new(
        max_attempts: NonZeroU32,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Result<Self, BuildError> {
        if initial_backoff > max_backoff {
            return Err(BuildError::new(
                BuildErrorKind::InvalidRetry,
                "initial retry backoff must not exceed the maximum",
            ));
        }
        Ok(Self {
            max_attempts,
            initial_backoff,
            max_backoff,
        })
    }

    #[must_use]
    pub const fn no_retry() -> Self {
        Self {
            max_attempts: NonZeroU32::MIN,
            initial_backoff: Duration::ZERO,
            max_backoff: Duration::ZERO,
        }
    }

    pub(super) fn backoff_after(&self, failed_attempt: u32) -> Duration {
        let exponent = failed_attempt.saturating_sub(1).min(31);
        let multiplier = 1_u32 << exponent;
        self.initial_backoff
            .checked_mul(multiplier)
            .unwrap_or(self.max_backoff)
            .min(self.max_backoff)
    }
}

impl Default for Retry {
    fn default() -> Self {
        Self::no_retry()
    }
}
