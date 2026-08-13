use deepseek_harness_cli::{
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, FinishReasonKind,
        FiniteNumber, JsonValue, JsonValueError, LlmCallConfig, LlmCallConfigAdapterDefaults,
        LlmFailure, Message, MessageRole, MessageSource, MessageSourceKind, ModelError,
        NonNegativeSafeInteger, ProviderRequestId, ReasoningEffortId, StreamChunk, TokenUsage,
        ToolSchema,
    },
    session::{EventSeq, SessionHeader, TurnEndCancelCause, TurnEndReason, TurnId, UnixMillis},
};
use serde_json::{Value, json};

fn json_value(value: Value) -> JsonValue {
    JsonValue::new(value).unwrap()
}

fn oracle() -> Value {
    serde_json::from_str(include_str!("fixtures/session/upstream_phase1_oracle.json")).unwrap()
}

#[test]
fn provider_neutral_messages_validate_role_source_and_tool_ids() {
    let user = Message::user(
        "user-1",
        vec![ContentBlock::text("hello").unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap();
    assert_eq!(user.role(), MessageRole::User);

    let assistant = Message::assistant(
        "assistant-1",
        vec![ContentBlock::reasoning("check").unwrap()],
        "mock",
        "mock-model",
    )
    .unwrap();
    assert_eq!(assistant.role(), MessageRole::Assistant);

    let result = Message::tool_result(
        "result-1",
        "call-1",
        vec![ContentBlock::text("ok").unwrap()],
        false,
    )
    .unwrap();
    let result_json = serde_json::to_value(&result).unwrap();
    assert_eq!(result_json["source"]["callId"], "call-1");
    assert_eq!(result_json["content"][0]["toolCallId"], "call-1");

    assert_eq!(
        Message::user("", vec![], MessageSource::user().unwrap()),
        Err(ModelError::EmptyMessageId)
    );
    assert_eq!(
        Message::assistant("assistant-2", vec![], "", "model"),
        Err(ModelError::EmptyProvider)
    );
}

#[test]
fn plugin_added_message_shapes_and_explicit_null_round_trip() {
    let raw = json!({
        "id": "plugin-message",
        "role": "user",
        "content": [{ "type": "plugin-block", "value": 1, "extra": null }],
        "source": { "kind": "plugin-source", "value": 2, "state": null },
        "messageExtra": { "kept": true }
    });
    let message = serde_json::from_value::<Message>(raw.clone()).unwrap();
    assert!(matches!(
        message.content()[0].kind(),
        ContentBlockKind::Other { block_type: Some(kind) } if kind == "plugin-block"
    ));
    assert!(matches!(
        message.source().kind(),
        MessageSourceKind::Other { kind } if kind == "plugin-source"
    ));
    assert_eq!(serde_json::to_value(message).unwrap(), raw);

    let source: MessageSource = serde_json::from_value(json!({
        "kind": "model",
        "provider": "mock",
        "model": "m",
        "replayState": null
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(source).unwrap()["replayState"],
        Value::Null
    );

    let loosely_typed_plugin = json!({
        "id": "plugin-known-kind",
        "role": "user",
        "content": [],
        "source": { "kind": "plugin", "plugin": "p", "form": null }
    });
    let message = serde_json::from_value::<Message>(loosely_typed_plugin.clone()).unwrap();
    assert!(matches!(
        message.source().kind(),
        MessageSourceKind::Other { kind } if kind == "plugin"
    ));
    assert_eq!(serde_json::to_value(message).unwrap(), loosely_typed_plugin);
}

#[test]
fn finish_and_turn_outcomes_remain_distinct() {
    let stream_reasons = [
        FinishReason::stop().unwrap(),
        FinishReason::tool_calls().unwrap(),
        FinishReason::max_tokens().unwrap(),
        FinishReason::aborted(LlmFailure::new("cancelled", "ABORTED").unwrap()).unwrap(),
        FinishReason::error(LlmFailure::new("offline", "NETWORK").unwrap()).unwrap(),
    ];
    for reason in stream_reasons {
        let encoded = serde_json::to_string(&reason).unwrap();
        assert_eq!(
            serde_json::from_str::<FinishReason>(&encoded).unwrap(),
            reason
        );
    }
    assert!(matches!(
        FinishReason::stop().unwrap().kind(),
        FinishReasonKind::Stop
    ));

    let turn_reasons = [
        TurnEndReason::Completed,
        TurnEndReason::Blocked,
        TurnEndReason::MaxTokens,
        TurnEndReason::Interrupted,
        TurnEndReason::Aborted {
            reason: TurnEndCancelCause::User,
        },
        TurnEndReason::Aborted {
            reason: TurnEndCancelCause::Hook {
                reason: "policy".to_owned(),
            },
        },
        TurnEndReason::Error {
            error: LlmFailure::new("provider failed", "UPSTREAM").unwrap(),
        },
    ];
    for reason in turn_reasons {
        let encoded = serde_json::to_string(&reason).unwrap();
        assert_eq!(
            serde_json::from_str::<TurnEndReason>(&encoded).unwrap(),
            reason
        );
    }
}

#[test]
fn llm_failure_rejects_facts_upstream_will_not_produce() {
    assert!(matches!(
        LlmFailure::new("", "CODE"),
        Err(ModelError::InvalidFailure(_))
    ));
    assert!(matches!(
        LlmFailure::new("message", ""),
        Err(ModelError::InvalidFailure(_))
    ));
    assert!(matches!(
        LlmFailure::from_parts("m".into(), "c".into(), Some(99), None, None),
        Err(ModelError::InvalidFailure(_))
    ));
    assert!(matches!(
        LlmFailure::from_parts(
            "m".into(),
            "c".into(),
            Some(503),
            None,
            Some(ProviderRequestId::new("")),
        ),
        Err(ModelError::InvalidFailure(_))
    ));
}

#[test]
fn all_known_content_stream_config_and_usage_shapes_round_trip() {
    let usage = TokenUsage::from_parts(
        NonNegativeSafeInteger::new(10).unwrap(),
        NonNegativeSafeInteger::new(5).unwrap(),
        Some(NonNegativeSafeInteger::new(2).unwrap()),
        Some(NonNegativeSafeInteger::new(1).unwrap()),
        Some(NonNegativeSafeInteger::new(3).unwrap()),
    )
    .unwrap();
    let blocks = vec![
        ContentBlock::text("visible").unwrap(),
        ContentBlock::reasoning("private reasoning").unwrap(),
        ContentBlock::tool_call("call-1", "read", r#"{"path":"a.rs"}"#).unwrap(),
        ContentBlock::tool_result(
            "call-1",
            vec![ContentBlock::text("contents").unwrap()],
            None,
        )
        .unwrap(),
    ];
    for block in &blocks {
        let encoded = serde_json::to_string(block).unwrap();
        assert_eq!(
            serde_json::from_str::<ContentBlock>(&encoded).unwrap(),
            *block
        );
        assert!(!encoded.contains("deepseek"));
    }

    let chunks = vec![
        StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        StreamChunk::text_delta(0, "hello").unwrap(),
        StreamChunk::reasoning_delta(1, "think").unwrap(),
        StreamChunk::tool_call_delta(2, "call-1", Some("read".into()), "{}").unwrap(),
        StreamChunk::block_end(3, blocks[0].clone()).unwrap(),
        StreamChunk::usage(usage).unwrap(),
        StreamChunk::finish(
            FinishReason::stop().unwrap(),
            Some(json_value(json!({ "cursor": 4 }))),
        )
        .unwrap(),
    ];
    for chunk in chunks {
        let encoded = serde_json::to_string(&chunk).unwrap();
        assert_eq!(
            serde_json::from_str::<StreamChunk>(&encoded).unwrap(),
            chunk
        );
    }

    let config = LlmCallConfig::from_parts(
        "mock".into(),
        "mock-model".into(),
        Some(ReasoningEffortId::new("high")),
        Some(FiniteNumber::new(0.25).unwrap()),
        Some(NonNegativeSafeInteger::new(8_192).unwrap()),
        Some(vec!["END".into()]),
    )
    .unwrap();
    let schema = ToolSchema::new(
        "read",
        "Read a file",
        json_value(json!({ "type": "object" })),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<LlmCallConfig>(&serde_json::to_string(&config).unwrap()).unwrap(),
        config
    );
    assert_eq!(
        serde_json::from_str::<ToolSchema>(&serde_json::to_string(&schema).unwrap()).unwrap(),
        schema
    );
}

#[test]
fn complete_model_vocabulary_matches_the_pinned_typescript_oracle() {
    let oracle = oracle();
    let vocabulary = &oracle["modelVocabulary"];

    let messages = vocabulary["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    for expected in messages {
        let parsed: Message = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), *expected);
    }

    let failure: LlmFailure = serde_json::from_value(vocabulary["failure"].clone()).unwrap();
    assert_eq!(failure.status(), Some(429));
    assert_eq!(failure.provider_retry_after_ms().unwrap().get(), 1_250.5);
    assert_eq!(failure.request_id().unwrap().as_str(), "request-oracle-1");
    assert_eq!(
        serde_json::to_value(failure).unwrap(),
        vocabulary["failure"]
    );

    for expected in vocabulary["finishReasons"].as_array().unwrap() {
        let parsed: FinishReason = serde_json::from_value(expected.clone()).unwrap();
        assert_eq!(serde_json::to_value(parsed).unwrap(), *expected);
    }

    let usage: TokenUsage = serde_json::from_value(vocabulary["tokenUsage"].clone()).unwrap();
    assert_eq!(usage.cache_read_tokens().unwrap().get(), 5);
    assert_eq!(usage.cache_write_tokens().unwrap().get(), 3);
    assert_eq!(usage.reasoning_tokens().unwrap().get(), 2);
    assert_eq!(
        serde_json::to_value(usage).unwrap(),
        vocabulary["tokenUsage"]
    );

    let mut token_delta_decisions = Vec::new();
    for expected in vocabulary["streamChunks"].as_array().unwrap() {
        let parsed: StreamChunk = serde_json::from_value(expected.clone()).unwrap();
        token_delta_decisions.push(parsed.is_token_delta());
        assert_eq!(serde_json::to_value(parsed).unwrap(), *expected);
    }
    assert_eq!(
        serde_json::to_value(token_delta_decisions).unwrap(),
        vocabulary["tokenDeltaDecisions"]
    );

    let config: LlmCallConfig = serde_json::from_value(vocabulary["callConfig"].clone()).unwrap();
    assert_eq!(config.temperature().unwrap().get(), 0.25);
    assert_eq!(config.stop().unwrap(), ["END", "STOP"]);
    assert_eq!(
        config.equivalent_to(&config.clone()),
        vocabulary["callConfigEquality"]["identical"]
            .as_bool()
            .unwrap()
    );
    let reordered = LlmCallConfig::from_parts(
        config.provider().to_owned(),
        config.model().to_owned(),
        config.reasoning_effort().cloned(),
        config.temperature(),
        config.max_tokens(),
        Some(vec!["STOP".to_owned(), "END".to_owned()]),
    )
    .unwrap();
    assert_eq!(
        config.equivalent_to(&reordered),
        vocabulary["callConfigEquality"]["changedStopOrder"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        serde_json::to_value(config).unwrap(),
        vocabulary["callConfig"]
    );

    let defaults: LlmCallConfigAdapterDefaults =
        serde_json::from_value(vocabulary["adapterDefaults"].clone()).unwrap();
    assert_eq!(
        serde_json::to_value(defaults).unwrap(),
        vocabulary["adapterDefaults"]
    );

    let schema: ToolSchema = serde_json::from_value(vocabulary["toolSchema"].clone()).unwrap();
    assert_eq!(schema.description(), "Read one file");
    assert!(schema.parameters().as_value().is_object());
    assert_eq!(
        serde_json::to_value(schema).unwrap(),
        vocabulary["toolSchema"]
    );
}

#[test]
fn safe_integer_types_accept_javascript_integer_notation_and_reject_rounding() {
    assert_eq!(serde_json::from_str::<EventSeq>("0.0").unwrap().get(), 0);
    assert_eq!(serde_json::from_str::<TurnId>("1e0").unwrap().get(), 1);
    assert_eq!(
        serde_json::from_str::<UnixMillis>("-1e0").unwrap().get(),
        -1
    );
    assert_eq!(
        serde_json::from_str::<NonNegativeSafeInteger>("9007199254740991")
            .unwrap()
            .get(),
        9_007_199_254_740_991
    );
    assert!(serde_json::from_str::<NonNegativeSafeInteger>("9007199254740992").is_err());
    assert!(
        serde_json::from_value::<TokenUsage>(json!({
            "inputTokens": 9_007_199_254_740_992_u64,
            "outputTokens": 0
        }))
        .is_err()
    );
    assert_eq!(
        JsonValue::new(json!(9_007_199_254_740_993_u64)),
        Err(JsonValueError::InvalidNumber)
    );
    let oracle = oracle();
    assert_eq!(
        oracle["numericBoundary"]["inputJson"],
        "{\"value\":9007199254740993}"
    );
    assert_eq!(
        oracle["numericBoundary"]["reencodedJson"],
        "{\"value\":9007199254740992}"
    );

    let ordered: JsonValue =
        serde_json::from_str(oracle["objectOrderBoundary"]["inputJson"].as_str().unwrap()).unwrap();
    assert_eq!(
        oracle["objectOrderBoundary"]["upstreamReencodedJson"],
        "{\"z\":1,\"a\":2}"
    );
    assert_eq!(
        serde_json::to_string(&ordered).unwrap(),
        "{\"a\":2,\"z\":1}"
    );
}

#[test]
fn public_header_deserialization_cannot_create_invalid_metadata() {
    assert!(
        serde_json::from_value::<SessionHeader>(json!({
            "version": 999,
            "id": "bad",
            "createdAt": 1
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<SessionHeader>(json!({
            "version": 0,
            "id": "bad",
            "createdAt": -1
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<SessionHeader>(json!({
            "version": 0,
            "id": "bad",
            "createdAt": 1,
            "cwd": null
        }))
        .is_err()
    );
}

#[test]
fn hostile_deep_json_is_rejected_without_recursive_drop() {
    let mut value = Value::Null;
    for _ in 0..=deepseek_harness_cli::model::MAX_JSON_DEPTH {
        value = Value::Array(vec![value]);
    }
    assert_eq!(
        JsonValue::new(value),
        Err(JsonValueError::TooDeep {
            maximum: deepseek_harness_cli::model::MAX_JSON_DEPTH
        })
    );
}
