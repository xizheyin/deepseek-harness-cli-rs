//! Stable provider failures and secret-safe HTTP classification.

use std::time::SystemTime;

use serde::Deserialize;

use crate::model::{
    FinishReason, LlmFailure, ModelError, PositiveFiniteNumber, ProviderRequestId, StreamChunk,
};

use super::{credentials::ApiKey, request::RequestBuildError};

const MAX_FAILURE_MESSAGE_BYTES: usize = 1_024;
const MAX_REQUEST_ID_BYTES: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct DeepSeekFailure {
    message: String,
    code: String,
    status: Option<u16>,
    retry_after_ms: Option<PositiveFiniteNumber>,
    request_id: Option<ProviderRequestId>,
}

impl DeepSeekFailure {
    pub(super) fn new(message: impl Into<String>, code: impl Into<String>) -> Self {
        let message = bounded_clean_message(&message.into());
        Self {
            message: if message.is_empty() {
                "DeepSeek request failed".to_owned()
            } else {
                message
            },
            code: code.into(),
            status: None,
            retry_after_ms: None,
            request_id: None,
        }
    }

    pub(super) fn from_request(error: &RequestBuildError) -> Self {
        Self::new(error.to_string(), error.code())
    }

    pub(super) fn cancelled() -> Self {
        Self::new("DeepSeek request was cancelled", "ABORTED")
    }

    pub(super) fn timeout() -> Self {
        Self::new("DeepSeek stream was idle for too long", "TIMEOUT")
    }

    pub(super) fn transport() -> Self {
        Self::new("DeepSeek transport failed", "TRANSPORT")
    }

    pub(super) fn http(
        status: u16,
        body: &[u8],
        retry_after: Option<&str>,
        request_id: Option<&str>,
        key: &ApiKey,
        now: SystemTime,
    ) -> Self {
        let provider = serde_json::from_slice::<WireErrorBody>(body)
            .ok()
            .and_then(|body| body.error);
        let detail = provider
            .as_ref()
            .map(|error| {
                [
                    error.code.as_deref(),
                    error.error_type.as_deref(),
                    error.message.as_deref(),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ")
            })
            .unwrap_or_default();
        let code = http_error_code(status, &detail);
        let fallback = format!("DeepSeek API error (HTTP {status})");
        let message = provider
            .and_then(|error| error.message)
            .filter(|message| !message.is_empty())
            .unwrap_or(fallback);
        let message = scrub_secret(&message, key);
        let status_fact = (100..=599).contains(&status).then_some(status);
        let retry_after_ms = retry_after.and_then(|value| parse_retry_after(value, now));
        let request_id = request_id
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= MAX_REQUEST_ID_BYTES
                    && value.bytes().all(|byte| (0x20..=0x7e).contains(&byte))
                    && !value.contains(key.expose())
                    && !value.to_ascii_lowercase().contains("bearer ")
            })
            .map(ProviderRequestId::new);
        Self {
            message,
            code,
            status: status_fact,
            retry_after_ms,
            request_id,
        }
    }

    pub(super) fn into_chunk(self) -> Result<StreamChunk, ModelError> {
        let failure = LlmFailure::from_parts(
            self.message,
            self.code.clone(),
            self.status,
            self.retry_after_ms,
            self.request_id,
        )?;
        let reason = if self.code == "ABORTED" {
            FinishReason::aborted(failure)?
        } else {
            FinishReason::error(failure)?
        };
        StreamChunk::finish(reason, None)
    }

    #[cfg(test)]
    pub(super) fn code(&self) -> &str {
        &self.code
    }

    #[cfg(test)]
    pub(super) fn message(&self) -> &str {
        &self.message
    }
}

fn http_error_code(status: u16, detail: &str) -> String {
    if status == 401 || status == 403 {
        return "AUTH".to_owned();
    }
    if is_quota(detail) {
        return "QUOTA".to_owned();
    }
    if status == 429 {
        return "RATE_LIMIT".to_owned();
    }
    if status == 400 {
        return if is_context_overflow(detail) {
            "CONTEXT_WINDOW_EXCEEDED".to_owned()
        } else {
            "INVALID_REQUEST".to_owned()
        };
    }
    if status >= 500 {
        return "SERVER".to_owned();
    }
    format!("HTTP_{status}")
}

fn normalized_words(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '_' | '-' => ' ',
            _ => character.to_ascii_lowercase(),
        })
        .collect()
}

fn is_quota(detail: &str) -> bool {
    let detail = normalized_words(detail);
    [
        "insufficient quota",
        "insufficient balance",
        "insufficient credit",
        "quota exceeded",
        "quota exhausted",
        "quota reached",
        "usage limit exceeded",
        "usage limit exhausted",
        "usage limit reached",
        "balance exhausted",
        "balance depleted",
        "credits exhausted",
        "credits depleted",
        "out of credits",
        "out of budget",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
        || ["exceeded quota", "exceeds quota", "exceed quota"]
            .iter()
            .any(|needle| detail.contains(needle))
        || [
            "exceeded your current quota",
            "exceeded the current quota",
            "exceeds your current quota",
            "exceeds the current quota",
        ]
        .iter()
        .any(|needle| detail.contains(needle))
}

fn is_context_overflow(detail: &str) -> bool {
    let detail = normalized_words(detail);
    [
        "context length exceeded",
        "context window exceeded",
        "context length overflow",
        "context window overflow",
        "maximum context length",
        "maximum context window",
        "max context length",
        "max context window",
        "too long for this model",
        "too large for this model",
        "too long for the model",
        "too large for the model",
        "too large for context",
        "too long for context",
        "input is larger than the model context",
        "prompt is larger than the model context",
        "request is larger than the model context",
        "messages are larger than the model context",
        "exceeds model context",
        "exceeded model context",
        "exceeds the model context",
        "exceeded the model context",
        "overflows model context",
        "overflows the model context",
    ]
    .iter()
    .any(|needle| detail.contains(needle))
}

fn parse_retry_after(value: &str, now: SystemTime) -> Option<PositiveFiniteNumber> {
    let millis = if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds = value.parse::<u64>().ok()?;
        if seconds == 0 {
            return None;
        }
        seconds.checked_mul(1_000)? as f64
    } else {
        let target = httpdate::parse_http_date(value).ok()?;
        let delay = target.duration_since(now).ok()?;
        let millis = u64::try_from(delay.as_millis()).ok()?;
        if millis == 0 {
            return None;
        }
        millis as f64
    };
    PositiveFiniteNumber::new(millis).ok()
}

fn scrub_secret(message: &str, key: &ApiKey) -> String {
    let mut scrubbed = message.replace(key.expose(), "[REDACTED]");
    let mut search_from = 0;
    loop {
        let lowercase = scrubbed.to_ascii_lowercase();
        let Some(relative) = lowercase[search_from..].find("bearer ") else {
            break;
        };
        let position = search_from + relative;
        let start = position + "bearer ".len();
        let tail = &scrubbed[start..];
        let end = tail
            .char_indices()
            .find_map(|(offset, character)| {
                (character.is_whitespace()
                    || matches!(character, '"' | '\'' | ',' | ';' | '}' | ']'))
                .then_some(start + offset)
            })
            .unwrap_or(scrubbed.len());
        scrubbed.replace_range(start..end, "[REDACTED]");
        search_from = start + "[REDACTED]".len();
    }
    let cleaned = bounded_clean_message(&scrubbed);
    if cleaned.is_empty() {
        "DeepSeek API request failed".to_owned()
    } else {
        cleaned
    }
}

fn bounded_clean_message(value: &str) -> String {
    let cleaned = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if cleaned.len() <= MAX_FAILURE_MESSAGE_BYTES {
        return cleaned;
    }
    let mut end = MAX_FAILURE_MESSAGE_BYTES;
    while !cleaned.is_char_boundary(end) {
        end -= 1;
    }
    cleaned[..end].to_owned()
}

#[derive(Debug, Deserialize)]
struct WireErrorBody {
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(default)]
    message: Option<String>,
    #[serde(default, rename = "type")]
    error_type: Option<String>,
    #[serde(default)]
    code: Option<String>,
}
