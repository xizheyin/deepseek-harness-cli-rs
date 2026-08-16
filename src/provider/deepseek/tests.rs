use std::{
    collections::{BTreeMap, VecDeque},
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, SystemTime},
};

use futures_util::{FutureExt, Stream, StreamExt, future::BoxFuture, stream};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    model::{
        ContentBlock, FinishReasonKind, FiniteNumber, JsonValue, LlmCallConfig,
        LlmCallConfigAdapterDefaults, Message, MessageSource, NonNegativeSafeInteger,
        ReasoningEffortId, StreamChunk, StreamChunkKind, ToolSchema,
    },
    provider::{
        MAX_PROVIDER_REQUEST_BYTES, PreparedProviderCall, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, RequestPurpose,
    },
};

use super::{
    adapter::DeepSeekProvider,
    config::{DEEPSEEK_PROVIDER, DeepSeekConfig, DeepSeekReasoningEffort, DeepSeekThinking},
    credentials::{
        ApiKey, CredentialLookup, CredentialRef, CredentialSource, SecretValue, StaticCredentials,
    },
    error::DeepSeekFailure,
    request::{RequestBuildError, request_value, serialize_request},
    response::{
        DONE, DeepSeekTranslator, MAX_DEEPSEEK_EMITTED_BYTES, MAX_DEEPSEEK_OUTPUT_BYTES,
        TranslateError,
    },
    sse::{
        MAX_DEEPSEEK_RESPONSE_BYTES, MAX_SSE_EVENT_BYTES, MAX_SSE_LINE_BYTES, SseDecoder, SseError,
        SseItem,
    },
    transport::{ByteStream, HttpRequest, HttpResponse, HttpTransport, TransportError},
};

const FAKE_KEY: &str = "sk-phase2-test-secret";

fn oracle() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/provider/upstream_phase2_oracle.json"
    ))
    .unwrap()
}

fn json_value(value: Value) -> JsonValue {
    JsonValue::new(value).unwrap()
}

fn prepared(config: LlmCallConfig) -> PreparedProviderCall {
    PreparedProviderCall::new(config, LlmCallConfigAdapterDefaults::default(), None)
}

fn simple_config() -> LlmCallConfig {
    LlmCallConfig::from_parts(
        DEEPSEEK_PROVIDER.to_owned(),
        "deepseek-chat".to_owned(),
        Some(ReasoningEffortId::new("high")),
        None,
        Some(NonNegativeSafeInteger::new(256_000).unwrap()),
        None,
    )
    .unwrap()
}

fn full_request() -> ProviderRequest {
    let messages = vec![
        Message::user(
            "message-user",
            vec![ContentBlock::text("weather in Paris?").unwrap()],
            MessageSource::user().unwrap(),
        )
        .unwrap(),
        Message::assistant(
            "message-assistant",
            vec![
                ContentBlock::reasoning("I should inspect two sources.").unwrap(),
                ContentBlock::tool_call("call-weather", "weather", r#"{"city":"Paris"}"#).unwrap(),
                ContentBlock::tool_call("call-clock", "clock", r#"{"zone":"Europe/Paris"}"#)
                    .unwrap(),
            ],
            DEEPSEEK_PROVIDER,
            "deepseek-v4-flash",
        )
        .unwrap(),
        Message::user(
            "message-tool",
            vec![
                ContentBlock::text("trusted note").unwrap(),
                ContentBlock::tool_result(
                    "call-weather",
                    vec![ContentBlock::text("sunny").unwrap()],
                    None,
                )
                .unwrap(),
                ContentBlock::tool_result("call-clock", vec![], None).unwrap(),
            ],
            MessageSource::plugin("phase2-oracle").unwrap(),
        )
        .unwrap(),
    ];
    let config = LlmCallConfig::from_parts(
        DEEPSEEK_PROVIDER.to_owned(),
        "deepseek-v4-flash".to_owned(),
        Some(ReasoningEffortId::new("max")),
        Some(FiniteNumber::new(0.2).unwrap()),
        Some(NonNegativeSafeInteger::new(128).unwrap()),
        Some(vec!["END".to_owned()]),
    )
    .unwrap();
    let tool = ToolSchema::new(
        "weather",
        "Read weather",
        json_value(json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"]
        })),
    )
    .unwrap();
    ProviderRequest::new(prepared(config), messages)
        .unwrap()
        .with_system("Be concise.")
        .unwrap()
        .with_tools(vec![tool])
        .unwrap()
}

fn simple_request() -> ProviderRequest {
    ProviderRequest::new(prepared(simple_config()), simple_messages()).unwrap()
}

fn simple_messages() -> Vec<Message> {
    vec![
        Message::user(
            "user-1",
            vec![ContentBlock::text("hello").unwrap()],
            MessageSource::user().unwrap(),
        )
        .unwrap(),
    ]
}

fn bound_simple_request(provider: &DeepSeekProvider) -> ProviderRequest {
    let prepared = provider
        .prepare_call(LlmCallConfig::new(DEEPSEEK_PROVIDER, "deepseek-chat").unwrap())
        .unwrap();
    ProviderRequest::new(prepared, simple_messages()).unwrap()
}

#[test]
fn request_serialization_matches_the_committed_upstream_oracle() {
    let request = full_request();
    let actual = request_value(&DeepSeekConfig::default(), &request).unwrap();
    assert_eq!(actual, oracle()["serialize"]["fullRequest"]["value"]);
    let encoded = serialize_request(&DeepSeekConfig::default(), &request).unwrap();
    assert_eq!(encoded, serde_json::to_vec(&actual).unwrap());
    assert_eq!(serde_json::from_slice::<Value>(&encoded).unwrap(), actual);

    let title = simple_request().with_purpose(RequestPurpose::SessionTitle);
    let value = request_value(&DeepSeekConfig::default(), &title).unwrap();
    assert_eq!(value["thinking"], json!({ "type": "disabled" }));
    assert!(value.get("reasoning_effort").is_none());
}

#[test]
fn preflight_counts_the_exact_wire_without_credentials_or_transport() {
    let transport = Arc::new(ScriptedTransport::new([]));
    let credentials = Arc::new(RotatingCredentials::default());
    let provider = provider_with(
        DeepSeekConfig::default(),
        credentials.clone(),
        transport.clone(),
    );
    let config = LlmCallConfig::new(DEEPSEEK_PROVIDER, "deepseek-chat").unwrap();
    let messages = simple_messages();
    let draft = ProviderRequestDraft::new(&config, &messages).unwrap();
    let preflight = provider.preflight_request(draft).unwrap();
    let encoded_bytes = preflight.encoded_bytes();
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
    assert_eq!(transport.request_count(), 0);

    let request = draft.into_request(preflight).unwrap();
    let encoded = serialize_request(&DeepSeekConfig::default(), &request).unwrap();
    assert_eq!(encoded.len(), encoded_bytes);
    assert_eq!(request.preflight_encoded_bytes(), Some(encoded_bytes));
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
    assert_eq!(transport.request_count(), 0);
}

#[test]
fn preflight_accepts_the_exact_wire_limit_and_rejects_one_more_byte() {
    let transport = Arc::new(ScriptedTransport::new([]));
    let credentials = Arc::new(RotatingCredentials::default());
    let provider = provider_with(
        DeepSeekConfig::default(),
        credentials.clone(),
        transport.clone(),
    );
    let config = LlmCallConfig::new(DEEPSEEK_PROVIDER, "deepseek-chat").unwrap();
    let messages = simple_messages();
    let empty = ProviderRequestDraft::new(&config, &messages)
        .unwrap()
        .with_system("")
        .unwrap();
    let base = provider.preflight_request(empty).unwrap().encoded_bytes();
    let remaining = MAX_PROVIDER_REQUEST_BYTES - base;
    let mut exact_system = "\0".repeat(remaining / 6);
    exact_system.push_str(&"x".repeat(remaining % 6));
    let exact = ProviderRequestDraft::new(&config, &messages)
        .unwrap()
        .with_system(&exact_system)
        .unwrap();
    assert_eq!(
        provider.preflight_request(exact).unwrap().encoded_bytes(),
        MAX_PROVIDER_REQUEST_BYTES
    );

    exact_system.push('x');
    let one_over = ProviderRequestDraft::new(&config, &messages)
        .unwrap()
        .with_system(&exact_system)
        .unwrap();
    assert!(matches!(
        provider.preflight_request(one_over),
        Err(ProviderPreflightError::WireTooLarge {
            maximum: MAX_PROVIDER_REQUEST_BYTES,
            ..
        })
    ));
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
    assert_eq!(transport.request_count(), 0);
}

#[test]
fn request_serialization_rejects_the_oracle_failure_cases() {
    let unsupported = LlmCallConfig::from_parts(
        DEEPSEEK_PROVIDER.to_owned(),
        "deepseek-chat".to_owned(),
        Some(ReasoningEffortId::new("medium")),
        None,
        None,
        None,
    )
    .unwrap();
    let request = ProviderRequest::new(prepared(unsupported), vec![]).unwrap();
    assert!(matches!(
        request_value(&DeepSeekConfig::default(), &request),
        Err(RequestBuildError::UnsupportedReasoningEffort { value }) if value == "medium"
    ));

    let image = crate::model::ImageAttachmentRef::new(
        format!("sha256:{}", "a".repeat(64)),
        crate::model::ImageMediaType::Png,
        68,
        1,
        1,
        None,
    )
    .unwrap();
    let message = Message::user(
        "image",
        vec![ContentBlock::image(image).unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap();
    let request = ProviderRequest::new(prepared(simple_config()), vec![message]).unwrap();
    assert_eq!(
        request_value(&DeepSeekConfig::default(), &request),
        Err(RequestBuildError::UnsupportedContent)
    );
}

fn interleaved_payloads() -> Vec<String> {
    vec![
        json!({ "choices": [{ "delta": { "role": "assistant", "content": null, "reasoning_content": "" } }] }).to_string(),
        json!({ "choices": [{ "delta": { "reasoning_content": "plan " } }] }).to_string(),
        json!({
            "choices": [{ "delta": {
                "reasoning_content": "first",
                "content": "Checking. ",
                "tool_calls": [
                    { "index": 7, "id": "call-a", "type": "function", "function": { "name": "one", "arguments": "{\"x\"" } },
                    { "index": 3, "id": "call-b", "type": "function", "function": { "name": "two", "arguments": "" } }
                ]
            } }]
        }).to_string(),
        json!({
            "choices": [{ "delta": {
                "content": "Done.",
                "tool_calls": [
                    { "index": 3, "function": { "arguments": "{\"y\":2}" } },
                    { "index": 7, "function": { "arguments": ":1}" } }
                ]
            } }]
        }).to_string(),
        json!({
            "choices": [{ "delta": {}, "finish_reason": "tool_calls" }],
            "usage": { "prompt_tokens": 100, "completion_tokens": 20, "prompt_cache_hit_tokens": 60 }
        }).to_string(),
        json!({
            "choices": [],
            "usage": {
                "prompt_tokens": 110,
                "completion_tokens": 21,
                "prompt_tokens_details": { "cached_tokens": 80 },
                "completion_tokens_details": { "reasoning_tokens": 5 }
            }
        }).to_string(),
        DONE.to_owned(),
    ]
}

#[test]
fn response_translation_matches_the_committed_upstream_oracle() {
    let mut translator = DeepSeekTranslator::default();
    let actual = interleaved_payloads()
        .iter()
        .flat_map(|payload| translator.accept(payload).unwrap())
        .map(|chunk| serde_json::to_value(chunk).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        Value::Array(actual),
        oracle()["translate"]["interleavedReasoningTextAndTools"]["chunks"]
    );
}

#[test]
fn response_translation_fails_closed_and_preserves_terminal_semantics() {
    let mut malformed = DeepSeekTranslator::default();
    assert_eq!(
        malformed.accept("{bad json"),
        Err(TranslateError::MalformedResponse)
    );

    let mut empty = DeepSeekTranslator::default();
    empty
        .accept(r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":0}}"#)
        .unwrap();
    let empty_chunks = empty
        .accept(DONE)
        .unwrap()
        .into_iter()
        .map(|chunk| serde_json::to_value(chunk).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        Value::Array(empty_chunks),
        oracle()["translate"]["completedWithoutContent"]["chunks"]
    );
    assert_eq!(empty.accept(DONE), Err(TranslateError::AfterDone));

    let mut unknown = DeepSeekTranslator::default();
    unknown
        .accept(r#"{"choices":[{"delta":{"content":"filtered"}}]}"#)
        .unwrap();
    unknown
        .accept(r#"{"choices":[{"delta":{},"finish_reason":"content_filter"}]}"#)
        .unwrap();
    let chunks = unknown
        .accept(DONE)
        .unwrap()
        .into_iter()
        .map(|chunk| serde_json::to_value(chunk).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        chunks.last().unwrap(),
        oracle()["translate"]["unknownFinishReason"]["chunks"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
    );
}

#[test]
fn translator_bounds_choices_tool_deltas_and_open_blocks() {
    let choices = json!({ "choices": vec![json!({ "delta": {} }); 129] }).to_string();
    assert!(matches!(
        DeepSeekTranslator::default().accept(&choices),
        Err(TranslateError::TooManyChoices { maximum: 128 })
    ));

    let calls = (0..129)
        .map(|index| json!({ "index": index }))
        .collect::<Vec<_>>();
    let tool_deltas = json!({
        "choices": [{ "delta": { "tool_calls": calls } }]
    })
    .to_string();
    assert!(matches!(
        DeepSeekTranslator::default().accept(&tool_deltas),
        Err(TranslateError::TooManyToolDeltas { maximum: 128 })
    ));

    let calls = (0..128)
        .map(|index| json!({ "index": index }))
        .collect::<Vec<_>>();
    let mut blocks = DeepSeekTranslator::default();
    blocks
        .accept(&json!({ "choices": [{ "delta": { "tool_calls": calls } }] }).to_string())
        .unwrap();
    assert!(matches!(
        blocks.accept(
            &json!({ "choices": [{ "delta": { "tool_calls": [{ "index": 128 }] } }] }).to_string()
        ),
        Err(TranslateError::TooManyBlocks { maximum: 128 })
    ));
}

#[test]
fn translator_bounds_retained_names_and_actual_emitted_chunk_bytes() {
    let large_name = "n".repeat(64 * 1024);
    let mut replacements = DeepSeekTranslator::default();
    for index in 0..80 {
        let payload = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-bounded",
                        "function": { "name": large_name, "arguments": "" }
                    }]
                }
            }]
        })
        .to_string();
        replacements.accept(&payload).unwrap_or_else(|error| {
            panic!("replacement {index} must not accumulate stale names: {error}")
        });
    }

    let huge_name = "x".repeat(1024 * 1024 - 512);
    let mut amplified = DeepSeekTranslator::default();
    let first = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": "call-amplified",
                    "function": { "name": huge_name, "arguments": "" }
                }]
            }
        }]
    })
    .to_string();
    amplified.accept(&first).unwrap();
    let tiny_wire =
        r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":""}}]}}]}"#;
    let error = (0..32)
        .find_map(|_| amplified.accept(tiny_wire).err())
        .expect("repeated cloning of one large name must hit the emitted-byte budget");
    assert!(matches!(error, TranslateError::EmittedTooLarge { .. }));
}

#[test]
fn translator_resource_limits_accept_exactly_the_budget_and_reject_one_more() {
    let mut retained = DeepSeekTranslator::default();
    let exact = json!({
        "choices": [{ "delta": { "content": "x".repeat(MAX_DEEPSEEK_OUTPUT_BYTES) } }]
    })
    .to_string();
    retained.accept(&exact).unwrap();
    assert!(matches!(
        retained.accept(r#"{"choices":[{"delta":{"content":"x"}}]}"#),
        Err(TranslateError::OutputTooLarge {
            maximum: MAX_DEEPSEEK_OUTPUT_BYTES
        })
    ));

    let initial_name = "n".repeat(1024 * 1024 - 512);
    let mut emitted = DeepSeekTranslator::default();
    let mut emitted_bytes = 0;
    for _ in 0..7 {
        let payload = json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-exact-budget",
                        "function": { "name": initial_name, "arguments": "" }
                    }]
                }
            }]
        })
        .to_string();
        emitted_bytes += emitted
            .accept(&payload)
            .unwrap()
            .iter()
            .map(|chunk| chunk.raw().encoded_len())
            .sum::<usize>();
    }
    let fixed =
        StreamChunk::tool_call_delta(0, "call-exact-budget", Some(String::new()), String::new())
            .unwrap()
            .raw()
            .encoded_len();
    let final_name_bytes = MAX_DEEPSEEK_EMITTED_BYTES - emitted_bytes - fixed;
    assert!(final_name_bytes <= MAX_DEEPSEEK_OUTPUT_BYTES);
    let final_payload = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "function": { "name": "z".repeat(final_name_bytes), "arguments": "" }
                }]
            }
        }]
    })
    .to_string();
    emitted_bytes += emitted
        .accept(&final_payload)
        .unwrap()
        .iter()
        .map(|chunk| chunk.raw().encoded_len())
        .sum::<usize>();
    assert_eq!(emitted_bytes, MAX_DEEPSEEK_EMITTED_BYTES);
    assert!(matches!(
        emitted.accept(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":""}}]}}]}"#
        ),
        Err(TranslateError::EmittedTooLarge {
            maximum: MAX_DEEPSEEK_EMITTED_BYTES
        })
    ));
}

fn decode_parts(parts: &[&[u8]]) -> Vec<SseItem> {
    let mut decoder = SseDecoder::default();
    let mut output = Vec::new();
    for part in parts {
        output.extend(decoder.push(part).unwrap());
    }
    output.extend(decoder.finish().unwrap());
    output
}

#[test]
fn sse_framing_is_independent_of_every_byte_boundary() {
    let bytes = b"\xef\xbb\xbf: pulse\r\ndata: one\rdata: two\r\rdata: snowman \xe2\x98\x83\n\ndata: bad \xff\n\n";
    let expected = vec![
        SseItem::Comment,
        SseItem::Data("one\ntwo".to_owned()),
        SseItem::Data("snowman \u{2603}".to_owned()),
        SseItem::Data("bad \u{fffd}".to_owned()),
    ];
    assert_eq!(decode_parts(&[bytes]), expected);
    let byte_parts = bytes.iter().map(std::slice::from_ref).collect::<Vec<_>>();
    assert_eq!(decode_parts(&byte_parts), expected);
    for split in 0..=bytes.len() {
        assert_eq!(decode_parts(&[&bytes[..split], &bytes[split..]]), expected);
    }
}

#[test]
fn sse_framing_matches_the_committed_upstream_oracle() {
    let observed = decode_parts(&[
        "\u{feff}: keep-alive\r".as_bytes(),
        "\ndata: {\"text\":\"你".as_bytes(),
        "好\"}\r\ndata: second\r\n\r\ndata: [DO".as_bytes(),
        "NE]\r\n\r\n".as_bytes(),
    ]);
    let expected = &oracle()["sse"]["fragmentedUtf8BomCrLfCommentAndMultiData"];
    let values = observed
        .iter()
        .filter_map(|item| match item {
            SseItem::Data(value) => Some(value.as_str()),
            SseItem::Comment => None,
        })
        .collect::<Vec<_>>();
    let comments = observed
        .iter()
        .filter(|item| matches!(item, SseItem::Comment))
        .count();
    assert_eq!(
        values,
        expected["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|value| value.as_str().unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(comments, expected["comments"].as_array().unwrap().len());

    let done = decode_parts(&[b"data: [DONE]\n\ndata: {\"late\":true}\n\n"]);
    assert_eq!(done, vec![SseItem::Data(DONE.to_owned())]);
    assert!(decode_parts(&[b"data: [DONE]"]).is_empty());
}

#[test]
fn sse_supports_multi_data_comments_and_blank_line_only_dispatch() {
    assert_eq!(
        decode_parts(&[b"event: message\n: keepalive\ndata:first\ndata: second\n\n"]),
        vec![SseItem::Comment, SseItem::Data("first\nsecond".to_owned())]
    );
    assert!(decode_parts(&[b"data: unfinished"]).is_empty());
    assert!(decode_parts(&[b"data: unfinished\n"]).is_empty());
    assert_eq!(
        decode_parts(&[b"data: [DONE]\n\n"]),
        vec![SseItem::Data(DONE.to_owned())]
    );
}

#[test]
fn sse_resource_limits_accept_exactly_the_budget_and_reject_one_more() {
    let mut line = SseDecoder::default();
    line.push(&vec![b'x'; MAX_SSE_LINE_BYTES]).unwrap();
    assert_eq!(
        line.push(b"x").unwrap_err().error,
        SseError::LineLength {
            maximum: MAX_SSE_LINE_BYTES,
        }
    );

    let first = MAX_SSE_EVENT_BYTES / 2;
    let second = MAX_SSE_EVENT_BYTES - first - 1;
    let exact_event = format!(
        "data:{}\ndata:{}\n\n",
        "a".repeat(first),
        "b".repeat(second)
    );
    let items = SseDecoder::default().push(exact_event.as_bytes()).unwrap();
    assert!(
        matches!(items.as_slice(), [SseItem::Data(value)] if value.len() == MAX_SSE_EVENT_BYTES)
    );
    let oversized_event = format!(
        "data:{}\ndata:{}\n\n",
        "a".repeat(first),
        "b".repeat(second + 1)
    );
    assert_eq!(
        SseDecoder::default()
            .push(oversized_event.as_bytes())
            .unwrap_err()
            .error,
        SseError::EventData {
            maximum: MAX_SSE_EVENT_BYTES,
        }
    );

    let mut response = SseDecoder::default();
    let complete_lines =
        (MAX_DEEPSEEK_RESPONSE_BYTES - MAX_SSE_LINE_BYTES) / (MAX_SSE_LINE_BYTES + 1);
    for _ in 0..complete_lines {
        response.push(&vec![b'x'; MAX_SSE_LINE_BYTES]).unwrap();
        response.push(b"\n").unwrap();
    }
    let used = complete_lines * (MAX_SSE_LINE_BYTES + 1);
    let mut remaining = MAX_DEEPSEEK_RESPONSE_BYTES - used;
    while remaining > 0 {
        let line_bytes = remaining.saturating_sub(1).min(MAX_SSE_LINE_BYTES);
        response.push(&vec![b'x'; line_bytes]).unwrap();
        remaining -= line_bytes;
        if remaining > 0 {
            response.push(b"\n").unwrap();
            remaining -= 1;
        }
    }
    assert_eq!(
        response.push(b"x").unwrap_err().error,
        SseError::ResponseSize {
            maximum: MAX_DEEPSEEK_RESPONSE_BYTES,
        }
    );

    let mut done_then_junk = Vec::from(&b"data: [DONE]\n\n"[..]);
    done_then_junk.extend(vec![b'x'; MAX_DEEPSEEK_RESPONSE_BYTES + 1]);
    assert_eq!(
        SseDecoder::default().push(&done_then_junk).unwrap(),
        vec![SseItem::Data(DONE.to_owned())]
    );
}

#[test]
fn sse_failure_keeps_complete_items_before_the_error_independent_of_read_boundaries() {
    let valid = b"data: {\"choices\":[]}\n\n";
    let oversized = vec![b'x'; MAX_SSE_LINE_BYTES + 1];
    let mut combined = Vec::from(valid);
    combined.extend_from_slice(&oversized);

    let failure = SseDecoder::default().push(&combined).unwrap_err();
    assert_eq!(
        failure.items,
        vec![SseItem::Data("{\"choices\":[]}".to_owned())]
    );
    assert_eq!(
        failure.error,
        SseError::LineLength {
            maximum: MAX_SSE_LINE_BYTES,
        }
    );

    let mut split = SseDecoder::default();
    assert_eq!(
        split.push(valid).unwrap(),
        vec![SseItem::Data("{\"choices\":[]}".to_owned())]
    );
    let split_failure = split.push(&oversized).unwrap_err();
    assert!(split_failure.items.is_empty());
    assert_eq!(split_failure.error, failure.error);
}

enum Script {
    Response(HttpResponse),
    Error,
    Pending,
}

#[derive(Default)]
struct ScriptedTransport {
    requests: Mutex<Vec<HttpRequest>>,
    scripts: Mutex<VecDeque<Script>>,
}

struct DropAwareBody {
    dropped: Arc<AtomicUsize>,
}

impl Stream for DropAwareBody {
    type Item = Result<Vec<u8>, TransportError>;

    fn poll_next(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Drop for DropAwareBody {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl ScriptedTransport {
    fn new(scripts: impl IntoIterator<Item = Script>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            scripts: Mutex::new(scripts.into_iter().collect()),
        }
    }

    fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl HttpTransport for ScriptedTransport {
    fn send(
        &self,
        request: HttpRequest,
        _cancellation: CancellationToken,
    ) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.requests.lock().unwrap().push(request);
        let script = self.scripts.lock().unwrap().pop_front();
        async move {
            match script {
                Some(Script::Response(response)) => Ok(response),
                Some(Script::Error) => Err(TransportError::new("scripted send failure")),
                Some(Script::Pending) => std::future::pending().await,
                None => Err(TransportError::new("missing test script")),
            }
        }
        .boxed()
    }
}

fn byte_stream(parts: Vec<Vec<u8>>) -> ByteStream {
    stream::iter(parts.into_iter().map(Ok)).boxed()
}

fn response(status: u16, headers: &[(&str, &str)], parts: Vec<Vec<u8>>) -> HttpResponse {
    HttpResponse::new(
        status,
        headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
        Some(byte_stream(parts)),
    )
}

fn success_bytes(text: &str) -> Vec<u8> {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{text:?}}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
    )
    .into_bytes()
}

fn static_credentials() -> Arc<dyn CredentialSource> {
    Arc::new(StaticCredentials::new(
        CredentialRef::new("DEEPSEEK_API_KEY").unwrap(),
        SecretValue::new(FAKE_KEY),
    ))
}

fn provider_with(
    config: DeepSeekConfig,
    credentials: Arc<dyn CredentialSource>,
    transport: Arc<dyn HttpTransport>,
) -> DeepSeekProvider {
    DeepSeekProvider::with_transport(config, credentials, transport)
}

async fn collect_chunks(
    provider: &DeepSeekProvider,
    cancellation: CancellationToken,
) -> Vec<StreamChunk> {
    provider
        .stream(bound_simple_request(provider), cancellation)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn terminal_failure(chunks: &[StreamChunk]) -> &crate::model::LlmFailure {
    let StreamChunkKind::Finish { reason, .. } = chunks.last().unwrap().kind() else {
        panic!("last provider chunk must be finish");
    };
    match reason.kind() {
        FinishReasonKind::Error { failure } | FinishReasonKind::Aborted { failure } => failure,
        other => panic!("expected failure finish, got {other:?}"),
    }
}

#[derive(Default)]
struct RotatingCredentials {
    calls: AtomicUsize,
}

impl CredentialSource for RotatingCredentials {
    fn resolve(&self, _reference: &CredentialRef) -> CredentialLookup {
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        CredentialLookup::Present(SecretValue::new(format!("sk-rotated-{call}")))
    }
}

#[tokio::test]
async fn fake_transport_is_lazy_sends_once_and_resolves_auth_per_call() {
    let transport = Arc::new(ScriptedTransport::new([
        Script::Response(response(200, &[], vec![success_bytes("one")])),
        Script::Response(response(200, &[], vec![success_bytes("two")])),
    ]));
    let credentials = Arc::new(RotatingCredentials::default());
    let provider = provider_with(
        DeepSeekConfig::default(),
        credentials.clone(),
        transport.clone(),
    );

    let mut first = provider.stream(bound_simple_request(&provider), CancellationToken::new());
    assert_eq!(
        transport.request_count(),
        0,
        "creating a stream must do no I/O"
    );
    assert!(first.next().await.unwrap().is_ok());
    while first.next().await.is_some() {}
    assert_eq!(transport.request_count(), 1);

    let second = collect_chunks(&provider, CancellationToken::new()).await;
    assert!(matches!(
        second.last().unwrap().kind(),
        StreamChunkKind::Finish { .. }
    ));
    assert_eq!(transport.request_count(), 2);
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 2);

    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer sk-rotated-1")
    );
    assert_eq!(
        requests[1].header("authorization"),
        Some("Bearer sk-rotated-2")
    );
    assert_eq!(requests[0].header("accept"), Some("text/event-stream"));
    assert_eq!(requests[0].header("content-type"), Some("application/json"));
    assert_eq!(requests[0].header("x-deepseek-harness-compact"), None);
    assert_eq!(
        serde_json::from_slice::<Value>(requests[0].body()).unwrap()["stream"],
        true
    );
    assert!(!format!("{:?}", requests[0]).contains("sk-rotated-1"));
}

#[tokio::test]
async fn prepared_call_is_bound_to_the_exact_provider_instance() {
    let transport_a = Arc::new(ScriptedTransport::new([]));
    let provider_a = provider_with(
        DeepSeekConfig::default(),
        static_credentials(),
        transport_a.clone(),
    );
    let transport_b = Arc::new(ScriptedTransport::new([]));
    let provider_b = provider_with(
        DeepSeekConfig::default(),
        static_credentials(),
        transport_b.clone(),
    );
    let config = LlmCallConfig::new(DEEPSEEK_PROVIDER, "deepseek-chat").unwrap();
    let messages = simple_messages();
    let draft = ProviderRequestDraft::new(&config, &messages).unwrap();
    let preflight = provider_a.preflight_request(draft).unwrap();
    let request = draft.into_request(preflight).unwrap();

    let chunks = provider_b
        .stream(request, CancellationToken::new())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(terminal_failure(&chunks).code(), "INVALID_PREPARED_CALL");
    assert_eq!(transport_a.request_count(), 0);
    assert_eq!(transport_b.request_count(), 0);
}

#[tokio::test]
async fn unbound_preparation_is_rejected_but_a_provider_clone_keeps_the_binding() {
    let transport = Arc::new(ScriptedTransport::new([Script::Response(response(
        200,
        &[],
        vec![success_bytes("from clone")],
    ))]));
    let provider = provider_with(
        DeepSeekConfig::default(),
        static_credentials(),
        transport.clone(),
    );

    let unbound = ProviderRequest::new(prepared(simple_config()), simple_messages()).unwrap();
    let rejected = provider
        .stream(unbound, CancellationToken::new())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(terminal_failure(&rejected).code(), "INVALID_PREPARED_CALL");
    assert_eq!(transport.request_count(), 0);

    let clone = provider.clone();
    let request = bound_simple_request(&provider);
    let chunks = clone
        .stream(request, CancellationToken::new())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(matches!(
        chunks.last().unwrap().kind(),
        StreamChunkKind::Finish { reason, .. }
            if matches!(reason.kind(), FinishReasonKind::Stop)
    ));
    assert_eq!(transport.request_count(), 1);
}

#[test]
fn public_prepare_call_rejects_wrong_routes_efforts_and_zero_max_tokens() {
    let transport = Arc::new(ScriptedTransport::new([]));
    let provider = provider_with(
        DeepSeekConfig::default(),
        static_credentials(),
        transport.clone(),
    );
    assert!(matches!(
        provider.prepare_call(LlmCallConfig::new("other", "model").unwrap()),
        Err(ProviderPrepareError::WrongProvider { expected, actual })
            if expected == DEEPSEEK_PROVIDER && actual == "other"
    ));

    let unsupported = LlmCallConfig::from_parts(
        DEEPSEEK_PROVIDER.to_owned(),
        "deepseek-chat".to_owned(),
        Some(ReasoningEffortId::new("medium")),
        None,
        None,
        None,
    )
    .unwrap();
    assert!(matches!(
        provider.prepare_call(unsupported),
        Err(ProviderPrepareError::UnsupportedReasoningEffort { value }) if value == "medium"
    ));

    let disabled = DeepSeekConfig::default()
        .with_thinking_defaults(
            Some(DeepSeekThinking::Disabled),
            DeepSeekReasoningEffort::Off,
        )
        .unwrap();
    let disabled_provider = provider_with(disabled, static_credentials(), transport.clone());
    let high = LlmCallConfig::from_parts(
        DEEPSEEK_PROVIDER.to_owned(),
        "deepseek-chat".to_owned(),
        Some(ReasoningEffortId::new("high")),
        None,
        None,
        None,
    )
    .unwrap();
    assert!(matches!(
        disabled_provider.prepare_call(high),
        Err(ProviderPrepareError::UnsupportedReasoningEffort { value }) if value == "high"
    ));

    let zero = LlmCallConfig::from_parts(
        DEEPSEEK_PROVIDER.to_owned(),
        "deepseek-chat".to_owned(),
        None,
        None,
        Some(NonNegativeSafeInteger::new(0).unwrap()),
        None,
    )
    .unwrap();
    assert!(matches!(
        provider.prepare_call(zero),
        Err(ProviderPrepareError::Model(crate::model::ModelError::InvalidShape {
            subject: "call config",
            detail,
        })) if detail == "maxTokens must be positive"
    ));
    assert_eq!(transport.request_count(), 0);
}

#[tokio::test]
async fn fake_http_statuses_map_to_stable_provider_failures() {
    let cases = [
        (401, "denied", "AUTH"),
        (402, "insufficient_quota", "QUOTA"),
        (429, "busy", "RATE_LIMIT"),
        (
            400,
            "maximum context length exceeded",
            "CONTEXT_WINDOW_EXCEEDED",
        ),
        (400, "bad input", "INVALID_REQUEST"),
        (503, "offline", "SERVER"),
        (418, "teapot", "HTTP_418"),
        (
            429,
            "You exceeded your current quota, please check your plan",
            "QUOTA",
        ),
        (
            400,
            "input is larger than the model context",
            "CONTEXT_WINDOW_EXCEEDED",
        ),
    ];
    for (status, message, expected) in cases {
        let body = json!({ "error": { "message": message } })
            .to_string()
            .into_bytes();
        let transport = Arc::new(ScriptedTransport::new([Script::Response(response(
            status,
            &[("x-request-id", "request-phase2")],
            vec![body],
        ))]));
        let provider = provider_with(
            DeepSeekConfig::default(),
            static_credentials(),
            transport.clone(),
        );
        let chunks = collect_chunks(&provider, CancellationToken::new()).await;
        let failure = terminal_failure(&chunks);
        assert_eq!(failure.code(), expected, "HTTP {status}");
        assert_eq!(failure.status(), Some(status));
        assert_eq!(failure.request_id().unwrap().as_str(), "request-phase2");
        assert_eq!(transport.request_count(), 1);
    }
}

#[test]
fn retry_after_parses_numeric_http_date_and_rejects_non_positive_or_invalid_values() {
    fn retry_after_ms(failure: DeepSeekFailure) -> Option<f64> {
        let chunks = [failure.into_chunk().unwrap()];
        terminal_failure(&chunks)
            .provider_retry_after_ms()
            .map(crate::model::PositiveFiniteNumber::get)
    }

    let key = ApiKey::normalize(SecretValue::new(FAKE_KEY)).unwrap();
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let body = br#"{"error":{"message":"busy"}}"#;
    let numeric = DeepSeekFailure::http(429, body, Some("3"), None, &key, now);
    assert_eq!(retry_after_ms(numeric), Some(3_000.0));

    let target = now + Duration::from_millis(2_000);
    let date = httpdate::fmt_http_date(target);
    let dated = DeepSeekFailure::http(429, body, Some(&date), None, &key, now);
    assert_eq!(retry_after_ms(dated), Some(2_000.0));

    for invalid in ["", "0", "not-a-date", "18446744073709551615"] {
        let failure = DeepSeekFailure::http(429, body, Some(invalid), None, &key, now);
        assert_eq!(retry_after_ms(failure), None, "{invalid:?}");
    }
}

#[tokio::test]
async fn request_id_prefers_primary_header_and_falls_back_to_deepseek_header() {
    let transport = Arc::new(ScriptedTransport::new([
        Script::Response(response(
            429,
            &[
                ("x-request-id", "primary-id"),
                ("x-deepseek-request-id", "fallback-must-not-win"),
            ],
            vec![],
        )),
        Script::Response(response(
            429,
            &[("x-deepseek-request-id", "deepseek-fallback")],
            vec![],
        )),
    ]));
    let provider = provider_with(DeepSeekConfig::default(), static_credentials(), transport);
    let primary = collect_chunks(&provider, CancellationToken::new()).await;
    assert_eq!(
        terminal_failure(&primary).request_id().unwrap().as_str(),
        "primary-id"
    );
    let fallback = collect_chunks(&provider, CancellationToken::new()).await;
    assert_eq!(
        terminal_failure(&fallback).request_id().unwrap().as_str(),
        "deepseek-fallback"
    );
}

#[tokio::test]
async fn send_and_missing_or_unreadable_http_bodies_have_stable_terminal_codes() {
    let unreadable = || stream::iter([Err(TransportError::new("scripted body failure"))]).boxed();
    let transport = Arc::new(ScriptedTransport::new([
        Script::Error,
        Script::Response(HttpResponse::new(200, BTreeMap::new(), None)),
        Script::Response(HttpResponse::new(429, BTreeMap::new(), None)),
        Script::Response(HttpResponse::new(429, BTreeMap::new(), Some(unreadable()))),
    ]));
    let provider = provider_with(
        DeepSeekConfig::default(),
        static_credentials(),
        transport.clone(),
    );
    for expected in ["TRANSPORT", "EMPTY_RESPONSE", "RATE_LIMIT", "RATE_LIMIT"] {
        let chunks = collect_chunks(&provider, CancellationToken::new()).await;
        assert_eq!(terminal_failure(&chunks).code(), expected);
    }
    assert_eq!(transport.request_count(), 4);
}

#[tokio::test]
async fn credential_failures_stop_before_http_without_exposing_the_value() {
    let cases = [
        (CredentialLookup::Missing, "MISSING_CREDENTIAL"),
        (CredentialLookup::InvalidEncoding, "INVALID_CREDENTIAL"),
        (
            CredentialLookup::Present(SecretValue::new("   ")),
            "INVALID_CREDENTIAL",
        ),
        (
            CredentialLookup::Present(SecretValue::new("secret with spaces")),
            "INVALID_CREDENTIAL",
        ),
    ];
    for (lookup, expected) in cases {
        struct FixedLookup(Mutex<Option<CredentialLookup>>);
        impl CredentialSource for FixedLookup {
            fn resolve(&self, _reference: &CredentialRef) -> CredentialLookup {
                self.0.lock().unwrap().take().unwrap()
            }
        }
        let transport = Arc::new(ScriptedTransport::new([]));
        let credentials: Arc<dyn CredentialSource> =
            Arc::new(FixedLookup(Mutex::new(Some(lookup))));
        let provider = provider_with(DeepSeekConfig::default(), credentials, transport.clone());
        let chunks = collect_chunks(&provider, CancellationToken::new()).await;
        assert_eq!(terminal_failure(&chunks).code(), expected);
        assert_eq!(transport.request_count(), 0);
        let encoded = serde_json::to_string(&chunks).unwrap();
        assert!(!encoded.contains("secret with spaces"));
    }
}

#[tokio::test]
async fn cancellation_latched_by_credential_lookup_overrides_its_failure() {
    struct CancellingLookup(CancellationToken);

    impl CredentialSource for CancellingLookup {
        fn resolve(&self, _reference: &CredentialRef) -> CredentialLookup {
            self.0.cancel();
            CredentialLookup::Missing
        }
    }

    let cancellation = CancellationToken::new();
    let transport = Arc::new(ScriptedTransport::new([]));
    let provider = provider_with(
        DeepSeekConfig::default(),
        Arc::new(CancellingLookup(cancellation.clone())),
        transport.clone(),
    );

    let chunks = collect_chunks(&provider, cancellation).await;
    assert_eq!(terminal_failure(&chunks).code(), "ABORTED");
    assert_eq!(transport.request_count(), 0);
}

#[tokio::test]
async fn live_encoder_failure_precedes_credentials_and_transport() {
    let credentials = Arc::new(RotatingCredentials::default());
    let transport = Arc::new(ScriptedTransport::new([]));
    let provider = provider_with(
        DeepSeekConfig::default(),
        credentials.clone(),
        transport.clone(),
    );
    let image = crate::model::ImageAttachmentRef::new(
        format!("sha256:{}", "a".repeat(64)),
        crate::model::ImageMediaType::Png,
        68,
        1,
        1,
        None,
    )
    .unwrap();
    let message = Message::user(
        "image-live",
        vec![ContentBlock::image(image).unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap();
    let prepared = provider
        .prepare_call(LlmCallConfig::new(DEEPSEEK_PROVIDER, "deepseek-chat").unwrap())
        .unwrap();
    let request = ProviderRequest::new(prepared, vec![message]).unwrap();

    let chunks = provider
        .stream(request, CancellationToken::new())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(terminal_failure(&chunks).code(), "UNSUPPORTED_CONTENT");
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
    assert_eq!(transport.request_count(), 0);
}

#[tokio::test]
async fn live_preflight_recipe_mismatch_fails_before_credentials_and_transport() {
    let credentials = Arc::new(RotatingCredentials::default());
    let transport = Arc::new(ScriptedTransport::new([]));
    let provider = provider_with(
        DeepSeekConfig::default(),
        credentials.clone(),
        transport.clone(),
    );
    let config = simple_config();
    let messages = simple_messages();
    let preflight_draft = ProviderRequestDraft::new(&config, &messages)
        .unwrap()
        .with_system("stable")
        .unwrap();
    let preflight = provider.preflight_request(preflight_draft).unwrap();
    let changed_draft = ProviderRequestDraft::new(&config, &messages)
        .unwrap()
        .with_system("stable\0")
        .unwrap();
    let request = changed_draft.into_request(preflight).unwrap();

    let chunks = provider
        .stream(request, CancellationToken::new())
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(terminal_failure(&chunks).code(), "INVALID_REQUEST");
    assert_eq!(credentials.calls.load(Ordering::SeqCst), 0);
    assert_eq!(transport.request_count(), 0);
}

#[tokio::test]
async fn cancelling_while_waiting_for_a_success_or_error_body_is_terminal() {
    for status in [200, 429] {
        let body = stream::pending::<Result<Vec<u8>, TransportError>>().boxed();
        let transport = Arc::new(ScriptedTransport::new([Script::Response(
            HttpResponse::new(status, BTreeMap::new(), Some(body)),
        )]));
        let provider = provider_with(DeepSeekConfig::default(), static_credentials(), transport);
        let cancellation = CancellationToken::new();
        let task_token = cancellation.clone();
        let task = tokio::spawn(async move { collect_chunks(&provider, task_token).await });
        tokio::task::yield_now().await;
        cancellation.cancel();
        let chunks = task.await.unwrap();
        assert_eq!(terminal_failure(&chunks).code(), "ABORTED", "HTTP {status}");
    }
}

#[tokio::test(start_paused = true)]
async fn success_and_error_body_reads_share_the_idle_timeout() {
    for status in [200, 429] {
        let body = stream::pending::<Result<Vec<u8>, TransportError>>().boxed();
        let transport = Arc::new(ScriptedTransport::new([Script::Response(
            HttpResponse::new(status, BTreeMap::new(), Some(body)),
        )]));
        let config = DeepSeekConfig::default()
            .with_stream_idle_timeout(Duration::from_millis(10))
            .unwrap();
        let provider = provider_with(config, static_credentials(), transport);
        let chunks = collect_chunks(&provider, CancellationToken::new()).await;
        assert_eq!(terminal_failure(&chunks).code(), "TIMEOUT", "HTTP {status}");
    }
}

#[tokio::test]
async fn dropping_the_consumer_drops_the_response_and_cancels_owned_work() {
    let dropped = Arc::new(AtomicUsize::new(0));
    let body = DropAwareBody {
        dropped: dropped.clone(),
    }
    .boxed();
    let transport = Arc::new(ScriptedTransport::new([Script::Response(
        HttpResponse::new(200, BTreeMap::new(), Some(body)),
    )]));
    let provider = provider_with(
        DeepSeekConfig::default(),
        static_credentials(),
        transport.clone(),
    );
    let stream = provider.stream(bound_simple_request(&provider), CancellationToken::new());
    let task = tokio::spawn(async move {
        let mut stream = stream;
        stream.next().await
    });
    while transport.request_count() == 0 {
        tokio::task::yield_now().await;
    }
    task.abort();
    let _ = task.await;
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn error_body_budget_accepts_the_limit_and_discards_one_byte_over() {
    fn body_with_len(length: usize) -> Vec<u8> {
        let prefix = br#"{"error":{"message":"insufficient quota","padding":""#;
        let suffix = br#""}}"#;
        assert!(length >= prefix.len() + suffix.len());
        let mut body = Vec::with_capacity(length);
        body.extend_from_slice(prefix);
        body.extend(vec![b'x'; length - prefix.len() - suffix.len()]);
        body.extend_from_slice(suffix);
        body
    }

    for (length, expected) in [(64 * 1024, "QUOTA"), (64 * 1024 + 1, "RATE_LIMIT")] {
        let transport = Arc::new(ScriptedTransport::new([Script::Response(response(
            429,
            &[],
            vec![body_with_len(length)],
        ))]));
        let provider = provider_with(DeepSeekConfig::default(), static_credentials(), transport);
        let chunks = collect_chunks(&provider, CancellationToken::new()).await;
        assert_eq!(terminal_failure(&chunks).code(), expected, "{length} bytes");
    }
}

#[tokio::test]
async fn cancellation_before_and_during_send_is_terminal_and_stops_work() {
    let not_started = Arc::new(ScriptedTransport::new([]));
    let provider = provider_with(
        DeepSeekConfig::default(),
        static_credentials(),
        not_started.clone(),
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let chunks = collect_chunks(&provider, cancelled).await;
    assert_eq!(terminal_failure(&chunks).code(), "ABORTED");
    assert_eq!(not_started.request_count(), 0);

    let pending = Arc::new(ScriptedTransport::new([Script::Pending]));
    let provider = provider_with(
        DeepSeekConfig::default(),
        static_credentials(),
        pending.clone(),
    );
    let cancellation = CancellationToken::new();
    let task_token = cancellation.clone();
    let task = tokio::spawn(async move { collect_chunks(&provider, task_token).await });
    tokio::task::yield_now().await;
    assert_eq!(pending.request_count(), 1);
    cancellation.cancel();
    let chunks = task.await.unwrap();
    assert_eq!(terminal_failure(&chunks).code(), "ABORTED");
}

#[tokio::test]
async fn cancellation_discards_translated_but_not_yet_published_chunks() {
    let body = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices": [{
                "delta": {
                    "reasoning_content": "plan",
                    "content": "answer",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-one",
                        "function": { "name": "tool", "arguments": "{}" }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
    )
    .into_bytes();
    let transport = Arc::new(ScriptedTransport::new([Script::Response(response(
        200,
        &[],
        vec![body],
    ))]));
    let provider = provider_with(DeepSeekConfig::default(), static_credentials(), transport);
    let cancellation = CancellationToken::new();
    let mut stream = provider.stream(bound_simple_request(&provider), cancellation.clone());
    let first = stream.next().await.unwrap().unwrap();
    assert!(matches!(first.kind(), StreamChunkKind::BlockStart { .. }));

    cancellation.cancel();
    let second = stream.next().await.unwrap().unwrap();
    let StreamChunkKind::Finish { reason, .. } = second.kind() else {
        panic!("cancellation must replace every queued provider chunk with one terminal finish");
    };
    assert!(matches!(
        reason.kind(),
        FinishReasonKind::Aborted { failure } if failure.code() == "ABORTED"
    ));
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn provider_chunk_budget_always_ends_with_a_terminal_failure() {
    let mut body = Vec::new();
    for _ in 0..(crate::provider::MAX_PROVIDER_STREAM_CHUNKS - 1) {
        body.extend_from_slice(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n");
    }
    let transport = Arc::new(ScriptedTransport::new([Script::Response(response(
        200,
        &[],
        vec![body],
    ))]));
    let provider = provider_with(DeepSeekConfig::default(), static_credentials(), transport);
    let chunks = collect_chunks(&provider, CancellationToken::new()).await;

    assert_eq!(chunks.len(), crate::provider::MAX_PROVIDER_STREAM_CHUNKS);
    assert_eq!(terminal_failure(&chunks).code(), "RESPONSE_TOO_LARGE");
}

#[tokio::test(start_paused = true)]
async fn an_idle_send_times_out_without_a_background_task() {
    let transport = Arc::new(ScriptedTransport::new([Script::Pending]));
    let config = DeepSeekConfig::default()
        .with_stream_idle_timeout(Duration::from_millis(10))
        .unwrap();
    let provider = provider_with(config, static_credentials(), transport.clone());
    let chunks = collect_chunks(&provider, CancellationToken::new()).await;
    assert_eq!(terminal_failure(&chunks).code(), "TIMEOUT");
    assert_eq!(transport.request_count(), 1);
}

#[tokio::test(start_paused = true)]
async fn sse_comments_refresh_the_idle_deadline() {
    let parts = vec![
        b": first\n\n".to_vec(),
        b": second\n\n".to_vec(),
        success_bytes("kept alive"),
    ];
    let delayed = stream::iter(parts).then(|bytes| async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        Ok(bytes)
    });
    let response = HttpResponse::new(200, BTreeMap::new(), Some(delayed.boxed()));
    let transport = Arc::new(ScriptedTransport::new([Script::Response(response)]));
    let config = DeepSeekConfig::default()
        .with_stream_idle_timeout(Duration::from_millis(8))
        .unwrap();
    let provider = provider_with(config, static_credentials(), transport);
    let chunks = collect_chunks(&provider, CancellationToken::new()).await;
    assert!(matches!(
        chunks.last().unwrap().kind(),
        StreamChunkKind::Finish { reason, .. } if matches!(reason.kind(), FinishReasonKind::Stop)
    ));
}

#[tokio::test]
async fn done_discards_later_sse_data_and_body_reads_are_backpressured() {
    let polls = Arc::new(AtomicUsize::new(0));
    let queue = Arc::new(Mutex::new(VecDeque::from([success_bytes("only")])));
    let body = {
        let polls = polls.clone();
        let queue = queue.clone();
        stream::poll_fn(move |_| {
            polls.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::Ready(queue.lock().unwrap().pop_front().map(Ok))
        })
        .boxed()
    };
    let transport = Arc::new(ScriptedTransport::new([Script::Response(
        HttpResponse::new(200, BTreeMap::new(), Some(body)),
    )]));
    let provider = provider_with(DeepSeekConfig::default(), static_credentials(), transport);
    let mut stream = provider.stream(bound_simple_request(&provider), CancellationToken::new());
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(stream.next().await.unwrap().is_ok());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    while stream.next().await.is_some() {}
    assert_eq!(
        polls.load(Ordering::SeqCst),
        1,
        "[DONE] must stop further body polling"
    );

    let later = format!(
        "{}data: {{bad json after done\n\n",
        String::from_utf8(success_bytes("first")).unwrap()
    )
    .into_bytes();
    let transport = Arc::new(ScriptedTransport::new([Script::Response(response(
        200,
        &[],
        vec![later],
    ))]));
    let provider = provider_with(DeepSeekConfig::default(), static_credentials(), transport);
    let chunks = collect_chunks(&provider, CancellationToken::new()).await;
    assert!(matches!(
        chunks.last().unwrap().kind(),
        StreamChunkKind::Finish { reason, .. } if matches!(reason.kind(), FinishReasonKind::Stop)
    ));
}

#[tokio::test]
async fn connection_loss_and_missing_done_become_terminal_failures() {
    let missing_done = b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n".to_vec();
    let transport = Arc::new(ScriptedTransport::new([Script::Response(response(
        200,
        &[],
        vec![missing_done],
    ))]));
    let provider = provider_with(DeepSeekConfig::default(), static_credentials(), transport);
    let chunks = collect_chunks(&provider, CancellationToken::new()).await;
    assert_eq!(terminal_failure(&chunks).code(), "STREAM_CLOSED");

    let body = stream::iter([Err(TransportError::new("test connection loss"))]).boxed();
    let transport = Arc::new(ScriptedTransport::new([Script::Response(
        HttpResponse::new(200, BTreeMap::new(), Some(body)),
    )]));
    let provider = provider_with(DeepSeekConfig::default(), static_credentials(), transport);
    let chunks = collect_chunks(&provider, CancellationToken::new()).await;
    assert_eq!(terminal_failure(&chunks).code(), "TRANSPORT");
}

#[test]
fn secrets_are_absent_from_failures_and_debug_output() {
    let key = ApiKey::normalize(SecretValue::new(FAKE_KEY)).unwrap();
    let failure = DeepSeekFailure::http(
        401,
        json!({
            "error": {
                "message": format!("credential {FAKE_KEY}; Bearer another-secret was rejected")
            }
        })
        .to_string()
        .as_bytes(),
        None,
        None,
        &key,
        SystemTime::UNIX_EPOCH,
    );
    assert_eq!(failure.code(), "AUTH");
    assert!(!failure.message().contains(FAKE_KEY));
    assert!(!failure.message().contains("another-secret"));
    assert!(failure.message().contains("[REDACTED]"));
    assert!(!format!("{failure:?}").contains(FAKE_KEY));
    assert!(!format!("{key:?}").contains(FAKE_KEY));
    assert!(!format!("{:?}", SecretValue::new(FAKE_KEY)).contains(FAKE_KEY));

    let mut request = HttpRequest::new(
        "https://api.deepseek.com/chat/completions".to_owned(),
        vec![],
    );
    request.insert_header("authorization", format!("Bearer {FAKE_KEY}"), true);
    assert!(!format!("{request:?}").contains(FAKE_KEY));
}

#[test]
fn durable_error_facts_remove_control_characters_and_invalid_request_ids() {
    let key = ApiKey::normalize(SecretValue::new(FAKE_KEY)).unwrap();
    let failure = DeepSeekFailure::http(
        429,
        br#"{"error":{"message":"first\nsecond\tthird"}}"#,
        None,
        Some("bad\nrequest-id"),
        &key,
        SystemTime::UNIX_EPOCH,
    );
    assert!(!failure.message().chars().any(char::is_control));
    let chunk = failure.into_chunk().unwrap();
    let StreamChunkKind::Finish { reason, .. } = chunk.kind() else {
        panic!("HTTP failure must become a finish chunk");
    };
    let FinishReasonKind::Error { failure } = reason.kind() else {
        panic!("HTTP failure must be an error finish");
    };
    assert!(failure.request_id().is_none());

    for reflected in [
        format!("gateway-{FAKE_KEY}-request"),
        "Bearer header-looking-secret".to_owned(),
    ] {
        let failure = DeepSeekFailure::http(
            429,
            br#"{"error":{"message":"rate limited"}}"#,
            None,
            Some(&reflected),
            &key,
            SystemTime::UNIX_EPOCH,
        )
        .into_chunk()
        .unwrap();
        let encoded = serde_json::to_string(&failure).unwrap();
        assert!(!encoded.contains(FAKE_KEY));
        assert!(!encoded.contains("header-looking-secret"));
        let StreamChunkKind::Finish { reason, .. } = failure.kind() else {
            panic!("HTTP failure must become a finish chunk");
        };
        let FinishReasonKind::Error { failure } = reason.kind() else {
            panic!("HTTP failure must be an error finish");
        };
        assert!(failure.request_id().is_none());
    }
}
