use crate::{
    model::{FiniteNumber, LlmFailure},
    provider::{RetryMode, RetryPolicy},
};

pub(crate) enum RetryDecision {
    Retry { delay_ms: FiniteNumber },
    NeedsSample,
    Stop,
}

pub(crate) fn decide(
    policy: &RetryPolicy,
    failure: &LlmFailure,
    retry: usize,
    sample: Option<f64>,
) -> RetryDecision {
    let eligible = match policy.mode() {
        RetryMode::Always => true,
        RetryMode::Normal => {
            let inside_policy = policy
                .max_retries()
                .and_then(|value| usize::try_from(value.get()).ok())
                .is_some_and(|maximum| retry <= maximum);
            inside_policy
                && policy
                    .retryable_codes()
                    .iter()
                    .any(|code| code == failure.code())
        }
    };
    if !eligible {
        return RetryDecision::Stop;
    }

    let backoff = policy.backoff();
    let maximum = backoff.max_delay_ms().get();
    if let Some(provider_delay) = failure.provider_retry_after_ms() {
        if provider_delay.get() <= maximum {
            let delay_ms = FiniteNumber::new(provider_delay.get()).ok();
            return delay_ms.map_or(RetryDecision::Stop, |delay_ms| RetryDecision::Retry {
                delay_ms,
            });
        }
        if policy.mode() == RetryMode::Normal {
            return RetryDecision::Stop;
        }
    }

    let Some(sample) = sample else {
        return RetryDecision::NeedsSample;
    };
    let exponent = i32::try_from(retry.saturating_sub(1).min(1_024)).unwrap_or(1_024);
    let base = (backoff.initial_delay_ms().get() * 2_f64.powi(exponent)).min(maximum);
    let ratio = backoff.jitter_ratio().get();
    let factor = 1.0 - ratio + 2.0 * ratio * sample;
    let delay = (base * factor).clamp(0.0, maximum);
    FiniteNumber::new(delay).map_or(RetryDecision::Stop, |delay_ms| RetryDecision::Retry {
        delay_ms,
    })
}

pub(crate) fn policy_key(policy: &RetryPolicy) -> Result<String, serde_json::Error> {
    let backoff = policy.backoff();
    let mut fields = vec![json_string(match policy.mode() {
        RetryMode::Always => "always",
        RetryMode::Normal => "normal",
    })?];
    if policy.mode() == RetryMode::Normal {
        fields.push(
            policy
                .max_retries()
                .map(|value| value.get().to_string())
                .unwrap_or_else(|| "null".to_owned()),
        );
        let mut codes = policy.retryable_codes().to_vec();
        codes.sort_by(|left, right| left.encode_utf16().cmp(right.encode_utf16()));
        let codes = codes
            .iter()
            .map(|code| json_string(code))
            .collect::<Result<Vec<_>, _>>()?
            .join(",");
        fields.push(format!("[{codes}]"));
    }
    let mut buffer = ryu_js::Buffer::new();
    fields.push(buffer.format(backoff.initial_delay_ms().get()).to_owned());
    fields.push(buffer.format(backoff.max_delay_ms().get()).to_owned());
    fields.push(buffer.format(backoff.jitter_ratio().get()).to_owned());
    Ok(format!("[{}]", fields.join(",")))
}

fn json_string(value: &str) -> Result<String, serde_json::Error> {
    serde_json::to_string(value)
}

#[cfg(test)]
mod tests {
    use super::policy_key;
    use crate::provider::{RetryBackoff, RetryPolicy};

    #[test]
    fn policy_key_uses_ecmascript_numbers_and_utf16_sorting() {
        let policy = RetryPolicy::normal(
            2,
            vec!["\u{e000}".to_owned(), "\u{10000}".to_owned()],
            RetryBackoff::new(0.000001, 0.00001, 0.0000001).unwrap(),
        )
        .unwrap();
        assert_eq!(
            policy_key(&policy).unwrap(),
            "[\"normal\",2,[\"𐀀\",\"\"],0.000001,0.00001,1e-7]"
        );
    }
}
