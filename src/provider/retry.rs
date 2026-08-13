//! Provider-owned retry facts frozen before one model call is logged.

use std::collections::HashSet;

use thiserror::Error;

use crate::model::{FiniteNumber, NonNegativeSafeInteger, PositiveFiniteNumber};

/// Largest timer delay used by the upstream JavaScript runtime.
pub const MAX_RETRY_DELAY_MILLIS: f64 = 2_147_483_647.0;
/// Maximum number of stable failure codes retained in one policy.
pub const MAX_RETRYABLE_CODES: usize = 256;
/// Maximum length of one stable failure code.
pub const MAX_RETRYABLE_CODE_BYTES: usize = 256;

const DEFAULT_RETRYABLE_CODES: [&str; 5] = [
    "EMPTY_RESPONSE",
    "RATE_LIMIT",
    "SERVER",
    "TIMEOUT",
    "TRANSPORT",
];

/// Whether retry execution is bounded or continues until cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetryMode {
    /// Retry selected transient failures up to a finite count.
    Normal,
    /// Retry every model-request failure until success or cancellation.
    Always,
}

/// Validated local exponential-backoff facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryBackoff {
    initial_delay_ms: PositiveFiniteNumber,
    max_delay_ms: PositiveFiniteNumber,
    jitter_ratio: FiniteNumber,
}

impl RetryBackoff {
    /// Validate one bounded backoff configuration.
    pub fn new(
        initial_delay_ms: f64,
        max_delay_ms: f64,
        jitter_ratio: f64,
    ) -> Result<Self, RetryPolicyError> {
        let initial_delay_ms = PositiveFiniteNumber::new(initial_delay_ms)
            .map_err(|_| RetryPolicyError::InvalidInitialDelay)?;
        let max_delay_ms = PositiveFiniteNumber::new(max_delay_ms)
            .map_err(|_| RetryPolicyError::InvalidMaxDelay)?;
        let jitter_ratio =
            FiniteNumber::new(jitter_ratio).map_err(|_| RetryPolicyError::InvalidJitter)?;
        if initial_delay_ms.get() > MAX_RETRY_DELAY_MILLIS {
            return Err(RetryPolicyError::InvalidInitialDelay);
        }
        if max_delay_ms.get() > MAX_RETRY_DELAY_MILLIS {
            return Err(RetryPolicyError::InvalidMaxDelay);
        }
        if initial_delay_ms > max_delay_ms {
            return Err(RetryPolicyError::InitialDelayAfterMaximum);
        }
        if !(0.0..=1.0).contains(&jitter_ratio.get()) {
            return Err(RetryPolicyError::InvalidJitter);
        }
        Ok(Self {
            initial_delay_ms,
            max_delay_ms,
            jitter_ratio,
        })
    }

    /// Initial local delay in milliseconds.
    #[must_use]
    pub fn initial_delay_ms(&self) -> PositiveFiniteNumber {
        self.initial_delay_ms
    }

    /// Largest local or provider-requested delay in milliseconds.
    #[must_use]
    pub fn max_delay_ms(&self) -> PositiveFiniteNumber {
        self.max_delay_ms
    }

    /// Symmetric random multiplier range around one.
    #[must_use]
    pub fn jitter_ratio(&self) -> FiniteNumber {
        self.jitter_ratio
    }
}

impl Default for RetryBackoff {
    fn default() -> Self {
        Self::new(500.0, 10_000.0, 0.1).expect("fixed retry defaults are valid")
    }
}

/// Immutable provider policy captured by a prepared model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetryPolicy {
    mode: RetryMode,
    max_retries: Option<NonNegativeSafeInteger>,
    retryable_codes: Vec<String>,
    backoff: RetryBackoff,
}

impl RetryPolicy {
    /// Build a bounded policy for selected stable failure codes.
    pub fn normal(
        max_retries: u64,
        retryable_codes: Vec<String>,
        backoff: RetryBackoff,
    ) -> Result<Self, RetryPolicyError> {
        let max_retries = NonNegativeSafeInteger::new(max_retries)
            .map_err(|_| RetryPolicyError::InvalidMaxRetries)?;
        validate_codes(&retryable_codes)?;
        Ok(Self {
            mode: RetryMode::Normal,
            max_retries: Some(max_retries),
            retryable_codes,
            backoff,
        })
    }

    /// Build a policy that delegates every failure to bounded Agent execution.
    #[must_use]
    pub fn always(backoff: RetryBackoff) -> Self {
        Self {
            mode: RetryMode::Always,
            max_retries: None,
            retryable_codes: Vec::new(),
            backoff,
        }
    }

    /// Retry mode selected by the provider.
    #[must_use]
    pub fn mode(&self) -> RetryMode {
        self.mode
    }

    /// Maximum retries after the first call in normal mode.
    #[must_use]
    pub fn max_retries(&self) -> Option<NonNegativeSafeInteger> {
        self.max_retries
    }

    /// Stable eligible failure codes in normal mode.
    #[must_use]
    pub fn retryable_codes(&self) -> &[String] {
        &self.retryable_codes
    }

    /// Frozen backoff facts.
    #[must_use]
    pub fn backoff(&self) -> &RetryBackoff {
        &self.backoff
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::normal(
            2,
            DEFAULT_RETRYABLE_CODES
                .into_iter()
                .map(str::to_owned)
                .collect(),
            RetryBackoff::default(),
        )
        .expect("fixed retry defaults are valid")
    }
}

fn validate_codes(codes: &[String]) -> Result<(), RetryPolicyError> {
    if codes.is_empty() {
        return Err(RetryPolicyError::EmptyCodes);
    }
    if codes.len() > MAX_RETRYABLE_CODES {
        return Err(RetryPolicyError::TooManyCodes {
            maximum: MAX_RETRYABLE_CODES,
            actual: codes.len(),
        });
    }
    let mut seen = HashSet::with_capacity(codes.len());
    for code in codes {
        if code.is_empty() || code.len() > MAX_RETRYABLE_CODE_BYTES {
            return Err(RetryPolicyError::InvalidCode);
        }
        if !seen.insert(code) {
            return Err(RetryPolicyError::DuplicateCode { code: code.clone() });
        }
    }
    Ok(())
}

/// Invalid provider-owned retry facts.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum RetryPolicyError {
    #[error("retry initial delay must be positive, finite, and inside the runtime timer range")]
    InvalidInitialDelay,
    #[error("retry maximum delay must be positive, finite, and inside the runtime timer range")]
    InvalidMaxDelay,
    #[error("retry initial delay must not exceed its maximum delay")]
    InitialDelayAfterMaximum,
    #[error("retry jitter ratio must be finite and between zero and one")]
    InvalidJitter,
    #[error("retry count must be a non-negative safe integer")]
    InvalidMaxRetries,
    #[error("normal retry policy must contain at least one eligible failure code")]
    EmptyCodes,
    #[error("retry failure codes must be non-empty and at most 256 bytes")]
    InvalidCode,
    #[error("retry policy has {actual} failure codes; maximum is {maximum}")]
    TooManyCodes { maximum: usize, actual: usize },
    #[error("retry failure code {code:?} appears more than once")]
    DuplicateCode { code: String },
}
