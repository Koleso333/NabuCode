use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures::executor::block_on;
use futures::{StreamExt, stream};
use ncore::core::codec;
use ncore::core::ir::{Accumulator, Block, StopReason};
use ncore::rt::{BoxFuture, header};
use ncore::{
    CancelToken, Core, Format, HttpRequest, HttpResponse, InboundRequest, PassthroughBody,
    PassthroughOptions, TranslateBody, TranslateOptions, Transport, TransportError,
};
use ncore_agentrouter_bp::AgentRouterBP;
use serde_json::{Value, json};

struct Mock {
    seen: Mutex<Vec<HttpRequest>>,
    body: Bytes,
    content_type: &'static str,
}

impl Mock {
    fn new(body: impl Into<Bytes>, content_type: &'static str) -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::default(),
            body: body.into(),
            content_type,
        })
    }
}

impl Transport for Mock {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.seen.lock().unwrap().push(req);
        let chunks: Vec<_> = self
            .body
            .chunks(17)
            .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
            .collect();
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), self.content_type.into())],
                body: Box::pin(stream::iter(chunks)),
            })
        })
    }
}

fn anthropic_request(streaming: bool) -> InboundRequest {
    InboundRequest {
        method: "POST".into(),
        path_and_query: "/v1/messages?beta=true".into(),
        headers: vec![
            ("x-api-key".into(), "client-key".into()),
            ("anthropic-version".into(), "2023-06-01".into()),
        ],
        body: Bytes::from(
            json!({
                "model": "client-model",
                "max_tokens": 128,
                "system": "Host prompt",
                "messages": [
                    {"role":"user","content":"Say hello using echo"},
                    {"role":"assistant","content":[
                        {"type":"tool_use","id":"call_previous","name":"echo","input":{"text":"hello"}}
                    ]},
                    {"role":"user","content":[
                        {"type":"tool_result","tool_use_id":"call_previous","content":"hello"}
                    ]}
                ],
                "tools": [{"name":"echo","input_schema":{
                    "type":"object","properties":{"text":{"type":"string"}},"required":["text"]
                }}],
                "stream": streaming
            })
            .to_string(),
        ),
    }
}

fn options() -> TranslateOptions {
    TranslateOptions {
        model: Some("gpt-6.1-sol".into()),
        ..Default::default()
    }
}

fn provider_response() -> Value {
    json!({
        "id":"resp_test","object":"response","status":"completed","model":"gpt-6.1-sol",
        "output":[{"type":"message","id":"msg_test","role":"assistant","status":"completed",
            "content":[{"type":"output_text","text":"hello","annotations":[]}]}],
        "usage":{"input_tokens":30,"output_tokens":2}
    })
}

#[test]
fn anthropic_request_and_response_use_core_translation() {
    let transport = Mock::new(provider_response().to_string(), "application/json");
    let core = Core::builder(transport.clone()).build();
    let response = block_on(core.translate(
        &AgentRouterBP::endpoint("provider-key"),
        anthropic_request(false),
        options(),
        &CancelToken::new(),
    ))
    .unwrap();

    let seen = transport.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let sent = &seen[0];
    assert_eq!(sent.url, "https://agentrouter.org/v1/responses");
    assert_eq!(header(&sent.headers, "originator"), Some("codex_cli_rs"));
    assert_eq!(
        header(&sent.headers, "user-agent"),
        Some(concat!(
            "nabucore-agentrouter-bp/",
            env!("CARGO_PKG_VERSION")
        ))
    );
    assert_eq!(
        header(&sent.headers, "authorization"),
        Some("Bearer provider-key")
    );
    assert_eq!(header(&sent.headers, "x-api-key"), None);
    assert_eq!(header(&sent.headers, "anthropic-version"), None);
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(body["model"], "gpt-6.1-sol");
    assert_eq!(body["instructions"], "Host prompt");
    assert_eq!(body["tools"][0]["name"], "echo");
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["type"] == "function_call_output"
                && item["call_id"] == "call_previous"
                && item["output"] == "hello")
    );

    assert_eq!(response.status, 200);
    let TranslateBody::Full { bytes, .. } = response.body else {
        panic!("expected full response");
    };
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][0]["text"], "hello");
    assert_eq!(body["stop_reason"], "end_turn");
}

#[test]
fn responses_stream_becomes_anthropic_tool_stream() {
    let transport = Mock::new(
        Bytes::from_static(include_bytes!(
            "fixtures/responses_tools.sse"
        )),
        "text/event-stream",
    );
    let core = Core::builder(transport.clone()).build();
    let response = block_on(core.translate(
        &AgentRouterBP::endpoint("provider-key"),
        anthropic_request(true),
        options(),
        &CancelToken::new(),
    ))
    .unwrap();
    assert_eq!(
        header(&response.headers, "content-type"),
        Some("text/event-stream")
    );
    let TranslateBody::Stream(mut stream) = response.body else {
        panic!("expected stream");
    };
    let bytes = block_on(async {
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        bytes
    });
    let mut parser = ncore::core::SseParser::new();
    let mut decoder = codec::stream_decoder(Format::AnthropicMessages);
    let mut result = Accumulator::new();
    for event in parser.feed(&bytes) {
        for event in decoder.feed(&event).unwrap() {
            result.push(&event);
        }
    }
    assert!(result.is_done());
    let result = result.finish();
    assert_eq!(result.stop, Some(StopReason::ToolUse));
    let tools: Vec<_> = result
        .content
        .iter()
        .filter_map(|block| match block {
            Block::ToolCall(call) => Some(call.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tools, ["get_weather"]);
    let seen = transport.seen.lock().unwrap();
    assert_eq!(header(&seen[0].headers, "originator"), Some("codex_cli_rs"));
}

#[test]
fn native_responses_preserve_body_and_replace_client_originator() {
    let reply = provider_response().to_string();
    let transport = Mock::new(reply.clone(), "application/json");
    let core = Core::builder(transport.clone()).build();
    let body = Bytes::from_static(
        br#"{"model":"gpt-6.1-sol","input":"hello","reasoning":{"effort":"high"},"service_tier":"priority","metadata":{"custom":"keep"}}"#,
    );
    let response = block_on(core.passthrough(
        &AgentRouterBP::endpoint("provider-key"),
        InboundRequest {
            method: "POST".into(),
            path_and_query: "/v1/responses".into(),
            headers: vec![
                ("Originator".into(), "other-client".into()),
                ("originator".into(), "duplicate".into()),
                ("User-Agent".into(), "host-agent/1.0".into()),
            ],
            body: body.clone(),
        },
        PassthroughOptions::default(),
        &CancelToken::new(),
    ))
    .unwrap();
    let seen = transport.seen.lock().unwrap();
    assert_eq!(seen[0].body, body);
    assert_eq!(
        header(&seen[0].headers, "user-agent"),
        Some("host-agent/1.0")
    );
    let originators: Vec<_> = seen[0]
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("originator"))
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(originators, ["codex_cli_rs"]);
    let PassthroughBody::Full { bytes, .. } = response.body else {
        panic!("expected full response");
    };
    assert_eq!(bytes.as_ref(), reply.as_bytes());
}
