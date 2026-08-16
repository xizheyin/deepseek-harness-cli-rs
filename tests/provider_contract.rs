use std::{sync::Arc, time::Duration};

use deepseek_harness_cli::{
    model::{
        ContentBlock, ContentBlockType, FinishReason, LlmCallConfig, LlmCallConfigAdapterDefaults,
        LlmFailure, Message, MessageSource, NonNegativeSafeInteger, StreamChunk, TokenUsage,
        ToolSchema,
    },
    provider::{
        MAX_PROVIDER_MESSAGES, MAX_PROVIDER_REQUEST_BYTES, MAX_PROVIDER_SESSION_ID_BYTES,
        MAX_PROVIDER_STREAM_CHUNKS, MAX_PROVIDER_TOOLS, MAX_RETRYABLE_CODE_BYTES,
        MAX_RETRYABLE_CODES, PreparedProviderCall, ProviderPreflightError, ProviderRequest,
        ProviderRequestDraft, ProviderRequestError, RetryBackoff, RetryMode, RetryPolicy,
        RetryPolicyError, StreamProtocolError, StreamValidator,
        deepseek::{
            CredentialLookup, CredentialRef, CredentialSource, DEEPSEEK_PROVIDER,
            DEFAULT_CONTEXT_WINDOW, DEFAULT_MAX_TOKENS, DeepSeekConfig, DeepSeekConfigError,
            DeepSeekModelConfig, DeepSeekProvider, DeepSeekReasoningEffort, DeepSeekThinking,
            MAX_DEEPSEEK_MODEL_ID_BYTES, MAX_DEEPSEEK_MODELS, SecretValue, StaticCredentials,
        },
    },
};
use serde_json::json;

fn call_config() -> LlmCallConfig {
    LlmCallConfig::new(DEEPSEEK_PROVIDER, "deepseek-chat").unwrap()
}

fn prepared_call() -> PreparedProviderCall {
    PreparedProviderCall::new(call_config(), LlmCallConfigAdapterDefaults::default(), None)
}

#[test]
fn fake_provider_preparation_can_publish_its_own_retry_policy() {
    let policy = RetryPolicy::always(RetryBackoff::new(1.5, 20.5, 0.0).unwrap());
    let prepared =
        PreparedProviderCall::new(call_config(), LlmCallConfigAdapterDefaults::default(), None)
            .with_retry_policy(policy.clone());
    assert_eq!(prepared.retry_policy(), &policy);
}

fn user_message(id: &str, text: &str) -> Message {
    Message::user(
        id,
        vec![ContentBlock::text(text).unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

fn tool_schema() -> ToolSchema {
    ToolSchema::new(
        "read",
        "Read one file",
        deepseek_harness_cli::model::JsonValue::new(json!({ "type": "object" })).unwrap(),
    )
    .unwrap()
}

fn usage() -> TokenUsage {
    TokenUsage::from_parts(
        NonNegativeSafeInteger::new(1).unwrap(),
        NonNegativeSafeInteger::new(1).unwrap(),
        None,
        None,
        None,
    )
    .unwrap()
}

#[test]
fn provider_request_is_bounded_before_wire_serialization() {
    let message = user_message("user-1", "hello");
    let request = ProviderRequest::new(prepared_call(), vec![message.clone()])
        .unwrap()
        .with_system("Be concise.")
        .unwrap()
        .with_tools(vec![tool_schema()])
        .unwrap()
        .with_session_id("session-01")
        .unwrap();

    assert_eq!(request.system(), Some("Be concise."));
    assert_eq!(request.messages(), [message]);
    assert_eq!(request.tools().len(), 1);
    assert_eq!(request.session_id().unwrap().as_str(), "session-01");
    assert!(request.retained_bytes() > 0);

    let private = "do-not-print-this-prompt";
    let debug = format!(
        "{:?}",
        ProviderRequest::new(prepared_call(), vec![user_message("private", private)])
            .unwrap()
            .with_system(private)
            .unwrap()
    );
    assert!(!debug.contains(private));

    let too_many_messages = vec![user_message("same-id", "x"); MAX_PROVIDER_MESSAGES + 1];
    assert!(matches!(
        ProviderRequest::new(prepared_call(), too_many_messages),
        Err(ProviderRequestError::TooManyMessages {
            maximum: MAX_PROVIDER_MESSAGES,
            actual,
        }) if actual == MAX_PROVIDER_MESSAGES + 1
    ));

    let too_many_tools = vec![tool_schema(); MAX_PROVIDER_TOOLS + 1];
    assert!(matches!(
        ProviderRequest::new(prepared_call(), vec![])
            .unwrap()
            .with_tools(too_many_tools),
        Err(ProviderRequestError::TooManyTools {
            maximum: MAX_PROVIDER_TOOLS,
            actual,
        }) if actual == MAX_PROVIDER_TOOLS + 1
    ));

    let oversized_system = "x".repeat(MAX_PROVIDER_REQUEST_BYTES + 1);
    assert!(matches!(
        ProviderRequest::new(prepared_call(), vec![])
            .unwrap()
            .with_system(oversized_system),
        Err(ProviderRequestError::TooLarge {
            maximum: MAX_PROVIDER_REQUEST_BYTES,
            ..
        })
    ));
}

#[test]
fn prepared_defaults_that_cross_the_retained_limit_remain_a_hard_preflight_limit() {
    let proposed = call_config();
    let exact_system = "x".repeat(
        MAX_PROVIDER_REQUEST_BYTES
            .checked_sub(proposed.raw().encoded_len())
            .unwrap(),
    );
    let exact = ProviderRequestDraft::new(&proposed, &[])
        .unwrap()
        .with_system(&exact_system)
        .unwrap();
    assert!(
        exact
            .finish(
                PreparedProviderCall::new(
                    proposed.clone(),
                    LlmCallConfigAdapterDefaults::default(),
                    None,
                ),
                1,
            )
            .is_ok()
    );

    let mut raw = proposed.raw().as_value().clone();
    raw["maxTokens"] = json!(1_024);
    let effective = serde_json::from_value(raw).unwrap();
    let prepared =
        PreparedProviderCall::new(effective, LlmCallConfigAdapterDefaults::default(), None);
    let grown = ProviderRequestDraft::new(&proposed, &[])
        .unwrap()
        .with_system(&exact_system)
        .unwrap()
        .finish(prepared, 1);
    assert!(matches!(
        grown,
        Err(ProviderPreflightError::RequestLimit { .. })
    ));
}

#[test]
fn provider_request_session_id_is_safe_for_one_http_header() {
    let request = || ProviderRequest::new(prepared_call(), vec![]).unwrap();

    assert!(request().with_session_id("session-1").is_ok());
    assert!(
        request()
            .with_session_id("x".repeat(MAX_PROVIDER_SESSION_ID_BYTES))
            .is_ok()
    );
    for invalid in [
        String::new(),
        "contains space".to_owned(),
        "contains\nnewline".to_owned(),
        "非 ASCII".to_owned(),
        "x".repeat(MAX_PROVIDER_SESSION_ID_BYTES + 1),
    ] {
        assert_eq!(
            request().with_session_id(invalid),
            Err(ProviderRequestError::InvalidSessionId)
        );
    }
}

#[test]
fn deepseek_endpoint_policy_allows_https_and_loopback_http_only() {
    let credential = CredentialRef::new("TEST_DEEPSEEK_API_KEY").unwrap();
    let default = DeepSeekConfig::default();
    assert_eq!(default.base_url(), "https://api.deepseek.com");
    assert_eq!(
        default.endpoint(),
        "https://api.deepseek.com/chat/completions"
    );

    let hosted = DeepSeekConfig::new("https://proxy.example/v1/", credential.clone()).unwrap();
    assert_eq!(hosted.base_url(), "https://proxy.example/v1");
    assert_eq!(
        hosted.endpoint(),
        "https://proxy.example/v1/chat/completions"
    );

    for base in [
        "http://127.0.0.1:8080",
        "http://localhost:8080/api",
        "http://[::1]:8080",
    ] {
        assert!(
            DeepSeekConfig::new(base, credential.clone()).is_ok(),
            "{base}"
        );
    }

    let rejected = [
        (
            "http://api.deepseek.com",
            DeepSeekConfigError::RemotePlainHttp,
        ),
        (
            "https://user:password@api.deepseek.com",
            DeepSeekConfigError::EndpointUserInfo,
        ),
        (
            "https://api.deepseek.com?route=other",
            DeepSeekConfigError::EndpointQueryOrFragment,
        ),
        (
            "https://api.deepseek.com#other",
            DeepSeekConfigError::EndpointQueryOrFragment,
        ),
        (
            "ftp://api.deepseek.com",
            DeepSeekConfigError::UnsupportedEndpointScheme,
        ),
        ("not a url", DeepSeekConfigError::InvalidEndpoint),
    ];
    for (base, expected) in rejected {
        assert_eq!(
            DeepSeekConfig::new(base, credential.clone()),
            Err(expected),
            "{base}"
        );
    }

    assert_eq!(
        default.clone().with_thinking_defaults(
            Some(DeepSeekThinking::Disabled),
            DeepSeekReasoningEffort::High,
        ),
        Err(DeepSeekConfigError::ThinkingDisabledWithEffort)
    );
    assert!(
        default
            .with_stream_idle_timeout(Duration::from_millis(1))
            .is_ok()
    );
}

#[test]
fn credential_references_are_validated_and_secret_debug_is_redacted() {
    for valid in ["DEEPSEEK_API_KEY", "_PRIVATE", "A1"] {
        assert_eq!(CredentialRef::new(valid).unwrap().as_str(), valid);
    }
    for invalid in ["", "1KEY", "HAS-DASH", "HAS SPACE", "非ASCII"] {
        assert!(CredentialRef::new(invalid).is_err(), "{invalid}");
    }

    let sentinel = "phase2-super-secret-sentinel";
    let reference = CredentialRef::new("TEST_DEEPSEEK_API_KEY").unwrap();
    let secret = SecretValue::new(sentinel);
    assert!(!format!("{secret:?}").contains(sentinel));

    let source = StaticCredentials::new(reference.clone(), secret);
    assert!(!format!("{source:?}").contains(sentinel));
    let lookup = source.resolve(&reference);
    assert!(!format!("{lookup:?}").contains(sentinel));
    assert!(matches!(lookup, CredentialLookup::Present(_)));
    assert!(matches!(
        source.resolve(&CredentialRef::new("OTHER_API_KEY").unwrap()),
        CredentialLookup::Missing
    ));

    let provider = deepseek_harness_cli::provider::deepseek::DeepSeekProvider::new(
        DeepSeekConfig::new("http://127.0.0.1:9", reference).unwrap(),
        Arc::new(source),
    )
    .unwrap();
    assert!(!format!("{provider:?}").contains(sentinel));
}

#[test]
fn deepseek_preparation_materializes_loggable_defaults_and_preserves_extensions() {
    let reference = CredentialRef::new("TEST_DEEPSEEK_API_KEY").unwrap();
    let provider = DeepSeekProvider::new(
        DeepSeekConfig::default(),
        Arc::new(StaticCredentials::new(
            reference,
            SecretValue::new("offline-fake-key"),
        )),
    )
    .unwrap();
    let proposal: LlmCallConfig = serde_json::from_value(json!({
        "provider": DEEPSEEK_PROVIDER,
        "model": "deepseek-chat",
        "futureFlag": { "kept": true }
    }))
    .unwrap();

    let prepared = provider.prepare_call(proposal).unwrap();
    assert_eq!(
        prepared.config().reasoning_effort().unwrap().as_str(),
        "high"
    );
    assert_eq!(
        prepared.config().max_tokens().unwrap().get(),
        DEFAULT_MAX_TOKENS
    );
    assert_eq!(
        prepared.context_window().unwrap().get(),
        DEFAULT_CONTEXT_WINDOW
    );
    assert!(prepared.adapter_defaults().reasoning_effort.is_some());
    assert!(prepared.adapter_defaults().max_tokens.is_some());
    assert_eq!(
        prepared.config().raw().as_value()["futureFlag"]["kept"],
        true
    );
}

#[test]
fn default_retry_policy_has_stable_bounded_values() {
    let policy = RetryPolicy::default();
    assert_eq!(policy.mode(), RetryMode::Normal);
    assert_eq!(policy.max_retries().unwrap().get(), 2);
    assert_eq!(
        policy.retryable_codes(),
        [
            "EMPTY_RESPONSE",
            "RATE_LIMIT",
            "SERVER",
            "TIMEOUT",
            "TRANSPORT",
        ]
    );
    assert_eq!(policy.backoff().initial_delay_ms().get(), 500.0);
    assert_eq!(policy.backoff().max_delay_ms().get(), 10_000.0);
    assert_eq!(policy.backoff().jitter_ratio().get(), 0.1);
}

#[test]
fn retry_backoff_and_code_lists_reject_ambiguous_or_unbounded_values() {
    assert_eq!(
        RetryBackoff::new(0.0, 1_000.0, 0.1),
        Err(RetryPolicyError::InvalidInitialDelay)
    );
    assert_eq!(
        RetryBackoff::new(2_000.0, 1_000.0, 0.1),
        Err(RetryPolicyError::InitialDelayAfterMaximum)
    );
    assert_eq!(
        RetryBackoff::new(500.0, 1_000.0, -0.1),
        Err(RetryPolicyError::InvalidJitter)
    );
    assert_eq!(
        RetryBackoff::new(500.0, 1_000.0, 1.1),
        Err(RetryPolicyError::InvalidJitter)
    );

    let backoff = RetryBackoff::default();
    assert_eq!(
        RetryPolicy::normal(1, vec![], backoff.clone()),
        Err(RetryPolicyError::EmptyCodes)
    );
    assert_eq!(
        RetryPolicy::normal(1, vec![String::new()], backoff.clone()),
        Err(RetryPolicyError::InvalidCode)
    );
    assert_eq!(
        RetryPolicy::normal(
            1,
            vec!["TRANSPORT".to_owned(), "TRANSPORT".to_owned()],
            backoff.clone(),
        ),
        Err(RetryPolicyError::DuplicateCode {
            code: "TRANSPORT".to_owned(),
        })
    );
    assert_eq!(
        RetryPolicy::normal(
            1,
            vec!["X".repeat(MAX_RETRYABLE_CODE_BYTES + 1)],
            backoff.clone(),
        ),
        Err(RetryPolicyError::InvalidCode)
    );
    assert!(matches!(
        RetryPolicy::normal(
            1,
            (0..=MAX_RETRYABLE_CODES)
                .map(|index| format!("CODE_{index}"))
                .collect(),
            backoff,
        ),
        Err(RetryPolicyError::TooManyCodes {
            maximum: MAX_RETRYABLE_CODES,
            actual,
        }) if actual == MAX_RETRYABLE_CODES + 1
    ));
}

#[test]
fn exact_model_limits_override_fallbacks_but_explicit_call_values_win() {
    let exact = DeepSeekModelConfig::new("exact-model")
        .unwrap()
        .with_context_window(32_000)
        .unwrap()
        .with_max_tokens(4_000)
        .unwrap();
    let config = DeepSeekConfig::default()
        .with_default_context_window(16_000)
        .unwrap()
        .with_default_max_tokens(2_000)
        .unwrap()
        .with_models(vec![exact])
        .unwrap();
    let reference = config.credential_ref().clone();
    let provider = DeepSeekProvider::new(
        config,
        Arc::new(StaticCredentials::new(
            reference,
            SecretValue::new("offline-fake-key"),
        )),
    )
    .unwrap();

    let exact = provider
        .prepare_call(LlmCallConfig::new(DEEPSEEK_PROVIDER, "exact-model").unwrap())
        .unwrap();
    assert_eq!(exact.config().max_tokens().unwrap().get(), 4_000);
    assert_eq!(exact.context_window().unwrap().get(), 32_000);
    assert!(exact.adapter_defaults().max_tokens.is_some());

    let explicit = LlmCallConfig::from_parts(
        DEEPSEEK_PROVIDER.to_owned(),
        "exact-model".to_owned(),
        None,
        None,
        Some(NonNegativeSafeInteger::new(777).unwrap()),
        None,
    )
    .unwrap();
    let explicit = provider.prepare_call(explicit).unwrap();
    assert_eq!(explicit.config().max_tokens().unwrap().get(), 777);
    assert_eq!(explicit.context_window().unwrap().get(), 32_000);
    assert!(explicit.adapter_defaults().max_tokens.is_none());

    let unlisted = provider
        .prepare_call(LlmCallConfig::new(DEEPSEEK_PROVIDER, "unlisted-model").unwrap())
        .unwrap();
    assert_eq!(unlisted.config().max_tokens().unwrap().get(), 2_000);
    assert_eq!(unlisted.context_window().unwrap().get(), 16_000);
}

#[test]
fn preparation_defaults_match_the_committed_upstream_oracle() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "fixtures/provider/upstream_phase2_oracle.json"
    ))
    .unwrap();
    let expected = &fixture["prepare"]["defaults"];
    let config = DeepSeekConfig::default();

    assert_eq!(
        serde_json::to_value(config.default_max_tokens()).unwrap(),
        expected["maxTokens"]
    );
    assert_eq!(
        serde_json::to_value(config.default_context_window()).unwrap(),
        expected["defaultContextWindow"]
    );
    let models = config
        .models()
        .iter()
        .map(|model| {
            json!({
                "id": model.id(),
                "name": model.name(),
                "contextWindow": model.context_window().map(|value| value.get()),
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(json!(models), expected["models"]);

    let policy = config.retry_policy();
    assert_eq!(policy.mode(), RetryMode::Normal);
    assert_eq!(
        policy.max_retries().unwrap().get(),
        expected["retryPolicy"]["maxRetries"].as_u64().unwrap()
    );
    assert_eq!(
        policy.retryable_codes(),
        expected["retryPolicy"]["retryableCodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        policy.backoff().initial_delay_ms().get(),
        expected["retryPolicy"]["initialDelayMs"].as_f64().unwrap()
    );
    assert_eq!(
        policy.backoff().max_delay_ms().get(),
        expected["retryPolicy"]["maxDelayMs"].as_f64().unwrap()
    );
    assert_eq!(
        policy.backoff().jitter_ratio().get(),
        expected["retryPolicy"]["jitterRatio"].as_f64().unwrap()
    );
}

#[test]
fn model_catalog_rejects_empty_duplicate_and_oversized_configuration() {
    assert_eq!(
        DeepSeekModelConfig::new(""),
        Err(DeepSeekConfigError::InvalidModelId)
    );
    assert_eq!(
        DeepSeekModelConfig::new("x".repeat(MAX_DEEPSEEK_MODEL_ID_BYTES + 1)),
        Err(DeepSeekConfigError::InvalidModelId)
    );
    assert_eq!(
        DeepSeekModelConfig::new("model").unwrap().with_name(""),
        Err(DeepSeekConfigError::InvalidModelName)
    );
    assert_eq!(
        DeepSeekModelConfig::new("model")
            .unwrap()
            .with_context_window(0),
        Err(DeepSeekConfigError::InvalidModelContextWindow)
    );
    assert_eq!(
        DeepSeekModelConfig::new("model")
            .unwrap()
            .with_max_tokens(0),
        Err(DeepSeekConfigError::InvalidModelMaxTokens)
    );

    let duplicate = DeepSeekModelConfig::new("duplicate").unwrap();
    assert_eq!(
        DeepSeekConfig::default().with_models(vec![duplicate.clone(), duplicate]),
        Err(DeepSeekConfigError::DuplicateModel {
            id: "duplicate".to_owned(),
        })
    );
    let too_many = (0..=MAX_DEEPSEEK_MODELS)
        .map(|index| DeepSeekModelConfig::new(format!("model-{index}")).unwrap())
        .collect();
    assert!(matches!(
        DeepSeekConfig::default().with_models(too_many),
        Err(DeepSeekConfigError::TooManyModels {
            maximum: MAX_DEEPSEEK_MODELS,
            actual,
        }) if actual == MAX_DEEPSEEK_MODELS + 1
    ));
}

#[test]
fn prepared_call_freezes_the_configured_retry_policy() {
    let configured = RetryPolicy::normal(
        7,
        vec!["RATE_LIMIT".to_owned(), "SERVER".to_owned()],
        RetryBackoff::new(250.0, 5_000.0, 0.25).unwrap(),
    )
    .unwrap();
    let config = DeepSeekConfig::default().with_retry_policy(configured.clone());
    let reference = config.credential_ref().clone();
    let provider = DeepSeekProvider::new(
        config,
        Arc::new(StaticCredentials::new(
            reference,
            SecretValue::new("offline-fake-key"),
        )),
    )
    .unwrap();

    let prepared = provider
        .prepare_call(LlmCallConfig::new(DEEPSEEK_PROVIDER, "deepseek-chat").unwrap())
        .unwrap();
    assert_eq!(prepared.retry_policy(), &configured);
}

#[test]
fn stream_validator_accepts_a_complete_text_stream() {
    let chunks = [
        StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        StreamChunk::text_delta(0, "hello").unwrap(),
        StreamChunk::block_end(0, ContentBlock::text("hello").unwrap()).unwrap(),
        StreamChunk::usage(usage()).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ];
    let mut validator = StreamValidator::default();
    for chunk in &chunks {
        validator.accept(chunk).unwrap();
    }
    assert_eq!(validator.chunk_count(), chunks.len());
    assert!(validator.is_finished());
    validator.complete().unwrap();
}

#[test]
fn stream_validator_rejects_orphans_type_mismatches_and_unknown_chunks() {
    let mut orphan_delta = StreamValidator::default();
    assert!(matches!(
        orphan_delta.accept(&StreamChunk::text_delta(0, "orphan").unwrap()),
        Err(StreamProtocolError::DeltaWithoutMatchingBlock { index: 0, .. })
    ));

    let mut wrong_delta = StreamValidator::default();
    wrong_delta
        .accept(&StreamChunk::block_start(0, ContentBlockType::Reasoning).unwrap())
        .unwrap();
    assert!(matches!(
        wrong_delta.accept(&StreamChunk::text_delta(0, "wrong").unwrap()),
        Err(StreamProtocolError::DeltaWithoutMatchingBlock { index: 0, .. })
    ));

    let mut orphan_end = StreamValidator::default();
    assert_eq!(
        orphan_end.accept(
            &StreamChunk::block_end(0, ContentBlock::text(String::new()).unwrap()).unwrap()
        ),
        Err(StreamProtocolError::BlockEndWithoutStart { index: 0 })
    );

    let mut mismatched_end = StreamValidator::default();
    mismatched_end
        .accept(&StreamChunk::block_start(0, ContentBlockType::Text).unwrap())
        .unwrap();
    assert!(matches!(
        mismatched_end.accept(
            &StreamChunk::block_end(0, ContentBlock::reasoning(String::new()).unwrap()).unwrap()
        ),
        Err(StreamProtocolError::BlockEndTypeMismatch { index: 0, .. })
    ));

    let unknown = StreamChunk::from_value(json!({ "type": "future-chunk" })).unwrap();
    assert_eq!(
        StreamValidator::default().accept(&unknown),
        Err(StreamProtocolError::UnknownLiveChunk {
            chunk_type: "future-chunk".to_owned(),
        })
    );
}

#[test]
fn stream_validator_requires_one_terminal_finish_in_the_right_position() {
    assert_eq!(
        StreamValidator::default().complete(),
        Err(StreamProtocolError::MissingFinish)
    );

    let mut duplicate_usage = StreamValidator::default();
    duplicate_usage
        .accept(&StreamChunk::usage(usage()).unwrap())
        .unwrap();
    assert_eq!(
        duplicate_usage.accept(&StreamChunk::usage(usage()).unwrap()),
        Err(StreamProtocolError::DuplicateUsage)
    );

    let mut open_success = StreamValidator::default();
    open_success
        .accept(&StreamChunk::block_start(0, ContentBlockType::Text).unwrap())
        .unwrap();
    assert_eq!(
        open_success.accept(&StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap()),
        Err(StreamProtocolError::SuccessfulFinishWithOpenBlocks { count: 1 })
    );

    let mut open_error = StreamValidator::default();
    open_error
        .accept(&StreamChunk::block_start(0, ContentBlockType::Text).unwrap())
        .unwrap();
    let error =
        FinishReason::error(LlmFailure::new("transport failed", "TRANSPORT").unwrap()).unwrap();
    open_error
        .accept(&StreamChunk::finish(error, None).unwrap())
        .unwrap();
    open_error.complete().unwrap();

    let mut after_finish = StreamValidator::default();
    after_finish
        .accept(&StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap())
        .unwrap();
    assert!(matches!(
        after_finish.accept(&StreamChunk::usage(usage()).unwrap()),
        Err(StreamProtocolError::ChunkAfterFinish { .. })
    ));
}

#[test]
fn stream_validator_rejects_closed_index_reuse_as_a_documented_rust_difference() {
    let mut validator = StreamValidator::default();
    validator
        .accept(&StreamChunk::block_start(0, ContentBlockType::Text).unwrap())
        .unwrap();
    validator
        .accept(&StreamChunk::block_end(0, ContentBlock::text("first").unwrap()).unwrap())
        .unwrap();

    assert_eq!(
        validator.accept(&StreamChunk::block_start(0, ContentBlockType::Reasoning).unwrap()),
        Err(StreamProtocolError::ReusedBlockIndex { index: 0 })
    );
}

#[test]
fn stream_validator_reserves_the_last_budget_slot_for_terminal_truth() {
    assert_eq!(MAX_PROVIDER_STREAM_CHUNKS % 2, 0);
    let mut validator = StreamValidator::default();
    for index in 0..((MAX_PROVIDER_STREAM_CHUNKS - 2) / 2) {
        let index = u64::try_from(index).unwrap();
        validator
            .accept(&StreamChunk::block_start(index, ContentBlockType::Text).unwrap())
            .unwrap();
        validator
            .accept(
                &StreamChunk::block_end(index, ContentBlock::text(String::new()).unwrap()).unwrap(),
            )
            .unwrap();
    }
    validator
        .accept(&StreamChunk::usage(usage()).unwrap())
        .unwrap();
    assert_eq!(validator.chunk_count(), MAX_PROVIDER_STREAM_CHUNKS - 1);
    assert_eq!(
        validator.accept(&StreamChunk::block_start(10_000, ContentBlockType::Text).unwrap()),
        Err(StreamProtocolError::TooManyChunks {
            maximum: MAX_PROVIDER_STREAM_CHUNKS,
        })
    );
    validator
        .accept(&StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap())
        .unwrap();
    assert_eq!(validator.chunk_count(), MAX_PROVIDER_STREAM_CHUNKS);
    validator.complete().unwrap();
}
