use bytes::Bytes;
use futures_util::{StreamExt, stream};
use ncore::core::{
    ErrorKind, codec,
    ir::{Accumulator, Block, Message, Request, Role, StopReason},
};
use ncore::rt::{BoxFuture, boxed, header};
use ncore::{
    CancelToken, Core, Endpoint, Format, GenerateOptions, Hook, HttpRequest, HttpResponse,
    InboundRequest, PassthroughBody, PassthroughOptions, RequestCtx, RetryPolicy, Transport,
    TransportError,
};
use ncore_deepaiacc::{
    ClientInfo, CredentialStore, Credentials, DeepAIAcc, MemoryStore, Session,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<HttpRequest>>,
    replies: Mutex<VecDeque<Result<HttpResponse, TransportError>>>,
}
impl Mock {
    fn push(&self, response: HttpResponse) {
        self.replies.lock().unwrap().push_back(Ok(response));
    }
    fn startup(&self) {
        self.push(allowance('a'));
        self.push(catalog());
    }
    fn requests(&self) -> Vec<HttpRequest> {
        self.seen.lock().unwrap().clone()
    }
}
impl Transport for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.seen.lock().unwrap().push(request);
        boxed(async {
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected network call")
        })
    }
}
fn response(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: Box::pin(stream::iter([Ok(Bytes::from(value.to_string()))])),
    }
}
fn allowance(nonce: char) -> HttpResponse {
    response(
        200,
        json!({"enabled":true,"balance":"78.00000","granted":"78","refresh_at":"2026-11-18T00:00:00Z","lane_version":7,"check_in":{"nonce":nonce.to_string().repeat(32),"seconds":600}}),
    )
}
fn catalog() -> HttpResponse {
    response(
        200,
        json!({"models":[
            {"id":"glm-5.3-flash","name":"GLM 5.3 Flash","reasoning":true,"input":["text","image"],"contextWindow":320000,"maxTokens":131072,"premier":false,"theosPerRequest":"0"},
            {"id":"glm-5.3","name":"GLM 5.3","reasoning":true,"input":["text","image"],"contextWindow":320000,"maxTokens":131072,"premier":true,"theosPerRequest":"3"},
            {"id":"muse-ember","name":"Muse Ember","reasoning":true,"input":["text"],"contextWindow":320000,"maxTokens":64000,"api":"openai-responses","premier":true,"theosPerRequest":"5"}
        ]}),
    )
}
fn events(values: &[Value], done: bool) -> HttpResponse {
    let mut text = values
        .iter()
        .map(|value| format!("data: {value}\n\n"))
        .collect::<String>();
    if done {
        text.push_str("data: [DONE]\n\n");
    }
    HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: Box::pin(stream::iter(
            text.as_bytes()
                .chunks(7)
                .map(|s| Ok(Bytes::copy_from_slice(s)))
                .collect::<Vec<_>>(),
        )),
    }
}
fn answer(done: bool) -> HttpResponse {
    events(
        &[
            json!({"id":"chat-1","model":"zai/glm-5.3","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Thought"}}]}),
            json!({"choices":[{"index":0,"delta":{"content":"OK"}}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":{"prompt_tokens":335,"completion_tokens":13,"total_tokens":348,"prompt_tokens_details":{"cached_tokens":0},"completion_tokens_details":{"reasoning_tokens":1}}}),
        ],
        done,
    )
}
fn credentials(key: &str) -> Credentials {
    Credentials::new(key).unwrap()
}
fn setup(mock: Arc<Mock>) -> (Core, Endpoint, Arc<DeepAIAcc>, Arc<MemoryStore>) {
    let store = Arc::new(MemoryStore::new(credentials("test-account-key")));
    let plugin = Arc::new(DeepAIAcc::new(store.clone(), ClientInfo::default()).unwrap());
    let session = Session::from_ids("test-session", "test-thread").unwrap();
    let endpoint = session
        .attach(DeepAIAcc::endpoint().with_plugin(plugin.clone()))
        .unwrap();
    (Core::builder(mock).build(), endpoint, plugin, store)
}
fn incoming(body: Value) -> InboundRequest {
    InboundRequest {
        method: "POST".into(),
        path_and_query: "/v1/chat/completions".into(),
        headers: vec![
            ("authorization".into(), "Bearer caller-secret".into()),
            ("api-key".into(), "caller-key".into()),
            ("cookie".into(), "caller-cookie".into()),
            ("user-agent".into(), "DeepAI-Code".into()),
            ("x-deepai-code-machine".into(), "caller-machine".into()),
        ],
        body: Bytes::from(body.to_string()),
    }
}
fn body(streaming: bool) -> Value {
    json!({"model":"glm-5.3-flash","stream":streaming,"max_completion_tokens":64,"reasoning_effort":"high","messages":[{"role":"system","content":"Caller instructions must stay verbatim."},{"role":"assistant","content":null,"reasoning_content":"previous thought","reasoning_details":[{"type":"reasoning.encrypted","data":"previous-signature","format":"test"}],"tool_calls":[{"id":"t1","type":"function","function":{"name":"read","arguments":"{\"path\":\"x\"}"}}]},{"role":"tool","tool_call_id":"t1","content":"full result"},{"role":"user","content":"Continue"}],"tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{}}}}],"tool_choice":"auto","store":true})
}
async fn drain(result: ncore::PassthroughResponse) -> Value {
    match result.body {
        PassthroughBody::Full { bytes, .. } => serde_json::from_slice(&bytes).unwrap(),
        PassthroughBody::Stream(mut body) => {
            let mut parser = ncore::core::SseParser::new();
            let mut decoder = codec::stream_decoder(Format::OpenAiChat);
            let mut accumulator = Accumulator::new();
            while let Some(chunk) = body.next().await {
                for event in parser.feed(&chunk.unwrap()) {
                    for item in decoder.feed(&event).unwrap() {
                        accumulator.push(&item);
                    }
                }
            }
            assert!(accumulator.is_done());
            serde_json::from_slice(
                &codec::encode_response(Format::OpenAiChat, &accumulator.finish()).0,
            )
            .unwrap()
        }
    }
}

#[tokio::test]
async fn metadata_reports_the_failed_operation_without_leaking_server_text() {
    let mock = Arc::new(Mock::default());
    mock.push(response(426, json!({"error":{"code":"invalid_api_key","message":"test-account-key must never be shown"}})));
    let (_, _, plugin, _) = setup(mock.clone());
    let error = plugin.snapshot(&*mock).await.unwrap_err();
    let ncore::Error::Provider(error) = error else {
        panic!("expected provider error")
    };
    assert_eq!(error.status, Some(426));
    assert_eq!(error.code.as_deref(), Some("invalid_api_key"));
    assert!(error.message.starts_with("/theos:"));
    assert!(!error.message.contains("test-account-key"));
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(
        header(&mock.requests()[0].headers, "x-deepai-code-theos"),
        Some("7")
    );
    assert_eq!(
        header(&mock.requests()[0].headers, "x-deepai-code-beacon"),
        Some("v=0.3.40;up=0")
    );

    let mock = Arc::new(Mock::default());
    mock.push(allowance('a'));
    mock.push(response(200, json!({"models":"malformed fixture"})));
    let (_, _, plugin, _) = setup(mock.clone());
    let error = plugin.snapshot(&*mock).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("/models: invalid DeepAI model catalog")
    );
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test]
async fn model_lane_rejection_updates_metadata_without_replaying_the_post() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(response(
        426,
        json!({"error":{"code":"lane_version_unsupported","message":"old lane"}}),
    ));
    mock.push(response(200, json!({"enabled":true,"balance":"78","granted":"78","lane_version":8,"check_in":{"nonce":"b".repeat(32),"seconds":600}})));
    for _ in 0..4 {
        mock.push(response(
            426,
            json!({"error":{"code":"lane_version_unsupported"}}),
        ));
    }
    mock.push(catalog());
    mock.push(response(200, json!({"enabled":true,"balance":"78","granted":"78","lane_version":8,"check_in":{"nonce":"c".repeat(32),"seconds":600}})));
    let (core, endpoint, plugin, store) = setup(mock.clone());
    let output = core
        .passthrough(
            &endpoint,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(output.status, 426);
    let PassthroughBody::Full { bytes, .. } = output.body else {
        panic!("error JSON expected")
    };
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["error"]["code"], "lane_updated");
    assert!(
        error["error"]["message"]
            .as_str()
            .unwrap()
            .contains("No model request was replayed")
    );
    assert_eq!(plugin.lane().await.unwrap(), 8);
    let saved = store.load().await.unwrap();
    assert_eq!(saved.lane, 8);
    assert_eq!(saved.check_in.unwrap().nonce, "c".repeat(32));
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
}

fn allowance_on(lane: u32, nonce: char) -> HttpResponse {
    response(
        200,
        json!({"enabled":true,"balance":"65","granted":"65","lane_version":lane,"check_in":{"nonce":nonce.to_string().repeat(32),"seconds":600}}),
    )
}

#[tokio::test]
async fn check_in_moves_to_the_lane_the_server_reports() {
    let mock = Arc::new(Mock::default());
    mock.push(allowance_on(8, 'a'));
    mock.push(allowance_on(8, 'b'));
    mock.push(catalog());
    let (_, _, plugin, store) = setup(mock.clone());
    plugin.snapshot(&*mock).await.unwrap();
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(header(&requests[0].headers, "x-deepai-code-theos"), Some("7"));
    assert_eq!(header(&requests[1].headers, "x-deepai-code-theos"), Some("8"));
    assert_eq!(header(&requests[1].headers, "x-deepai-code-check-in"), None);
    assert_eq!(header(&requests[2].headers, "x-deepai-code-theos"), Some("8"));
    assert_eq!(plugin.lane().await.unwrap(), 8);
    let saved = store.load().await.unwrap();
    assert_eq!(saved.lane, 8);
    assert_eq!(saved.check_in.unwrap().nonce, "b".repeat(32));
}

#[tokio::test]
async fn check_in_stops_when_the_server_keeps_changing_the_lane() {
    let mock = Arc::new(Mock::default());
    mock.push(allowance_on(8, 'a'));
    mock.push(allowance_on(9, 'b'));
    let (_, _, plugin, _) = setup(mock.clone());
    let error = plugin.snapshot(&*mock).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("/theos: DeepAI returned lane 9 after switching to lane 8")
    );
    assert_eq!(mock.requests().len(), 2);
    assert_eq!(plugin.lane().await.unwrap(), 8);
}

#[tokio::test]
async fn failed_lane_renewal_after_a_model_response_is_not_a_quota_failover() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(response(
        426,
        json!({"error":{"code":"lane_version_unsupported"}}),
    ));
    mock.push(response(200, json!({"enabled":true,"balance":"78","granted":"78","lane_version":8,"check_in":{"nonce":"b".repeat(32),"seconds":600}})));
    for _ in 0..4 {
        mock.push(response(
            426,
            json!({"error":{"code":"lane_version_unsupported"}}),
        ));
    }
    mock.push(catalog());
    mock.push(response(
        429,
        json!({"error":{"code":"rate_limit_exceeded"}}),
    ));
    let (core, endpoint, plugin, _) = setup(mock.clone());
    let error = core
        .passthrough(
            &endpoint,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ncore::Error::Plugin { .. }));
    assert!(error.to_string().contains("no model request was replayed"));
    assert_eq!(plugin.lane().await.unwrap(), 8);
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
}

#[tokio::test]
async fn successful_http_metadata_with_an_error_never_authorizes_a_model() {
    for operation in ["/theos", "/models"] {
        let mock = Arc::new(Mock::default());
        if operation == "/theos" {
            mock.push(response(200, json!({"error":{"code":"theos_exhausted"},"enabled":true,"balance":"78","granted":"78","lane_version":7,"check_in":{"nonce":"a".repeat(32),"seconds":600}})));
        } else {
            mock.push(allowance('a'));
            let mut value: Value =
                serde_json::from_slice(&catalog().body.next().await.unwrap().unwrap()).unwrap();
            value["error"] = json!({"code":"lane_version_unsupported"});
            mock.push(response(200, value));
        }
        let (core, endpoint, plugin, _) = setup(mock.clone());
        let error = core
            .passthrough(
                &endpoint,
                incoming(body(false)),
                PassthroughOptions::default(),
                &CancelToken::new(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ncore::Error::Provider(_)));
        assert_eq!(plugin.lane().await.unwrap(), 7);
        assert!(
            mock.requests()
                .iter()
                .all(|request| request.method == "GET")
        );
        assert_eq!(
            mock.requests().len(),
            if operation == "/theos" { 1 } else { 2 }
        );
    }
}

#[tokio::test]
async fn native_headers_history_and_session_are_preserved() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(answer(true));
    let (core, endpoint, plugin, store) = setup(mock.clone());
    let original = body(true);
    let result = core
        .passthrough(
            &endpoint,
            incoming(original.clone()),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        drain(result).await["choices"][0]["message"]["content"],
        "OK"
    );
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].url.ends_with("/theos"));
    assert!(requests[1].url.ends_with("/models"));
    assert_eq!(
        requests[2].url,
        "https://api.deepai.org/deepai_code/chat/completions"
    );
    let sent: Value = serde_json::from_slice(&requests[2].body).unwrap();
    for field in [
        "messages",
        "tools",
        "tool_choice",
        "reasoning_effort",
        "max_completion_tokens",
    ] {
        assert_eq!(sent[field], original[field]);
    }
    assert_eq!(sent["store"], false);
    assert_eq!(sent["stream_options"]["include_usage"], true);
    assert_eq!(sent["prompt_cache_key"], "test-thread");
    let saved = store.load().await.unwrap();
    assert_eq!(saved.check_in.as_ref().unwrap().nonce, "a".repeat(32));
    for request in &requests {
        assert_eq!(
            header(&request.headers, "api-key"),
            Some("test-account-key")
        );
        assert_eq!(header(&request.headers, "user-agent"), Some("node"));
        assert_eq!(header(&request.headers, "x-deepai-code-theos"), Some("7"));
        assert_eq!(
            header(&request.headers, "x-deepai-code-machine"),
            Some(saved.machine_digest().as_str())
        );
        for field in ["authorization", "cookie", "x-deepai-cache-key"] {
            assert!(header(&request.headers, field).is_none());
        }
    }
    assert_eq!(
        header(&requests[2].headers, "x-session-affinity"),
        Some("test-session")
    );
    assert!(header(&requests[2].headers, "x-deepai-code-check-in").is_none());
    let usage = plugin.last_usage().unwrap();
    assert_eq!(usage.raw["prompt_tokens"], 335);
    assert_eq!(usage.raw["prompt_tokens_details"]["cached_tokens"], 0);
}

#[tokio::test]
async fn nonstream_collects_sse_and_preserves_native_usage() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(answer(true));
    let (core, ep, plugin, _) = setup(mock.clone());
    let result = core
        .passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let value = drain(result).await;
    assert_eq!(value["choices"][0]["message"]["content"], "OK");
    assert_eq!(
        value["usage"]["completion_tokens_details"]["reasoning_tokens"],
        1
    );
    assert_eq!(plugin.last_usage().unwrap().raw["total_tokens"], 348);
    assert_eq!(
        serde_json::from_slice::<Value>(&mock.requests()[2].body).unwrap()["stream"],
        true
    );
}

#[tokio::test]
async fn nonstream_rejects_truncated_sse() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(answer(false));
    let (core, ep, _, _) = setup(mock.clone());
    assert!(
        core.passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(mock.requests().len(), 3);
}

#[tokio::test]
async fn metadata_and_next_turn_reuse_check_in_and_catalog() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(answer(true));
    mock.push(answer(true));
    let (core, ep, plugin, _) = setup(mock.clone());
    let snapshot = plugin.snapshot(&*mock).await.unwrap();
    assert_eq!(
        snapshot.catalog.model("glm-5.3").unwrap().context_window,
        320000
    );
    assert_eq!(snapshot.allowance.balance.unwrap().as_str(), "78.00000");
    for _ in 0..2 {
        let result = core
            .passthrough(
                &ep,
                incoming(body(false)),
                PassthroughOptions::default(),
                &CancelToken::new(),
            )
            .await
            .unwrap();
        drain(result).await;
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.url.ends_with("/theos"))
            .count(),
        1
    );
    for request in &requests[2..] {
        assert_eq!(
            header(&request.headers, "x-session-affinity"),
            Some("test-session")
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&request.body).unwrap()["prompt_cache_key"],
            "test-thread"
        );
    }
}

#[tokio::test]
async fn saved_nonce_is_resumed_and_replaced_even_on_refusal() {
    let mock = Arc::new(Mock::default());
    mock.push(response(403,json!({"error":{"code":"network_bound","message":"must not expose caller-secret"},"check_in":{"nonce":"b".repeat(32),"seconds":600}})));
    let (_, _, plugin, store) = setup(mock.clone());
    let mut saved = store.load().await.unwrap();
    saved.check_in = Some(ncore_deepaiacc::CheckIn {
        nonce: "a".repeat(32),
        seconds: 600.0,
        checked_at: 1,
    });
    store.save(&saved).await.unwrap();
    let error = plugin.check_in(&*mock).await.unwrap_err();
    let ncore::Error::Provider(error) = error else {
        panic!("provider error expected")
    };
    assert_eq!(error.code.as_deref(), Some("network_bound"));
    assert_eq!(error.kind, ncore::core::ErrorKind::Billing);
    assert!(!format!("{error:?}").contains("caller-secret"));
    assert_eq!(
        store.load().await.unwrap().check_in.unwrap().nonce,
        "b".repeat(32)
    );
    assert_eq!(
        header(&mock.requests()[0].headers, "x-deepai-code-check-in"),
        Some("a".repeat(32).as_str())
    );
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn malformed_allowance_does_not_overwrite_saved_nonce() {
    let mock = Arc::new(Mock::default());
    mock.push(response(200,json!({"enabled":true,"balance":"bad","granted":"78","check_in":{"nonce":"b".repeat(32),"seconds":600}})));
    let (_, _, plugin, store) = setup(mock.clone());
    let mut saved = store.load().await.unwrap();
    saved.check_in = Some(ncore_deepaiacc::CheckIn {
        nonce: "a".repeat(32),
        seconds: 600.0,
        checked_at: 1,
    });
    store.save(&saved).await.unwrap();
    assert!(plugin.check_in(&*mock).await.is_err());
    assert_eq!(
        store.load().await.unwrap().check_in.unwrap().nonce,
        "a".repeat(32)
    );
}

#[tokio::test]
async fn concurrent_catalog_requests_share_check_in() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(catalog());
    let (_, _, plugin, _) = setup(mock.clone());
    let (a, b) = tokio::join!(plugin.models(&*mock), plugin.models(&*mock));
    assert!(a.is_ok() && b.is_ok());
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.url.ends_with("/theos"))
            .count(),
        1
    );
}

#[tokio::test]
async fn switched_account_fails_before_network() {
    let mock = Arc::new(Mock::default());
    mock.push(allowance('a'));
    let (_, _, plugin, store) = setup(mock.clone());
    plugin.credits(&*mock).await.unwrap();
    store.save(&credentials("another-test-key")).await.unwrap();
    assert!(plugin.credits(&*mock).await.is_err());
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn two_accounts_keep_identity_nonce_and_balance_separate() {
    let first = Arc::new(Mock::default());
    let second = Arc::new(Mock::default());
    first.push(allowance('a'));
    second.push(allowance('b'));
    let (_, _, a, sa) = setup(first.clone());
    let sb = Arc::new(MemoryStore::new(credentials("second-key")));
    let b = DeepAIAcc::new(sb.clone(), ClientInfo::default()).unwrap();
    tokio::try_join!(a.credits(&*first), b.credits(&*second)).unwrap();
    let a = sa.load().await.unwrap();
    let b = sb.load().await.unwrap();
    assert_ne!(a.machine_digest(), b.machine_digest());
    assert_ne!(a.instance, b.instance);
    assert_eq!(a.check_in.unwrap().nonce, "a".repeat(32));
    assert_eq!(b.check_in.unwrap().nonce, "b".repeat(32));
}

#[tokio::test]
async fn endpoint_pinning_and_invalid_session_fail_before_network() {
    let mock = Arc::new(Mock::default());
    let (core, ep, _, _) = setup(mock.clone());
    for path in [
        "/v1/models",
        "/v1/chat/completions?leak=1",
        "/../chat/completions",
    ] {
        let mut req = incoming(body(true));
        req.path_and_query = path.into();
        assert!(
            core.passthrough(&ep, req, PassthroughOptions::default(), &CancelToken::new())
                .await
                .is_err()
        );
    }
    let mut ep = ep;
    ep.base_url = "https://example.com".into();
    assert!(
        core.passthrough(
            &ep,
            incoming(body(true)),
            PassthroughOptions::default(),
            &CancelToken::new()
        )
        .await
        .is_err()
    );
    assert!(Session::from_ids("bad\r\nkey", "thread").is_err());
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn unsupported_effort_and_output_budget_do_not_call_model() {
    for (field, value) in [
        ("reasoning_effort", json!("xhigh")),
        ("max_completion_tokens", json!(131073)),
        ("model", json!("unknown-model")),
    ] {
        let mock = Arc::new(Mock::default());
        mock.startup();
        let (core, ep, _, _) = setup(mock.clone());
        let mut input = body(false);
        input[field] = value;
        assert!(
            core.passthrough(
                &ep,
                incoming(input),
                PassthroughOptions::default(),
                &CancelToken::new()
            )
            .await
            .is_err()
        );
        assert_eq!(mock.requests().len(), 2);
    }
}

#[tokio::test]
async fn off_maps_to_none_without_clamping_high_levels() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(answer(true));
    let (core, ep, _, _) = setup(mock.clone());
    let mut input = body(false);
    input["reasoning_effort"] = "off".into();
    let result = core
        .passthrough(
            &ep,
            incoming(input),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    drain(result).await;
    assert_eq!(
        serde_json::from_slice::<Value>(&mock.requests()[2].body).unwrap()["reasoning_effort"],
        "none"
    );
}

#[tokio::test]
async fn developer_role_changes_only_role() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(answer(true));
    let (core, ep, _, _) = setup(mock.clone());
    let mut input = body(false);
    input["messages"][0]["role"] = "developer".into();
    let result = core
        .passthrough(
            &ep,
            incoming(input.clone()),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    drain(result).await;
    let sent: Value = serde_json::from_slice(&mock.requests()[2].body).unwrap();
    assert_eq!(sent["messages"][0]["role"], "system");
    assert_eq!(
        sent["messages"][0]["content"],
        input["messages"][0]["content"]
    );
}

#[tokio::test]
async fn model_stream_slot_is_released_when_caller_drops_response() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(answer(true));
    mock.push(answer(true));
    let (core, ep, _, _) = setup(mock.clone());
    let cancel = CancelToken::new();
    let first = core
        .passthrough(
            &ep,
            incoming(body(true)),
            PassthroughOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
    let second = core.passthrough(
        &ep,
        incoming(body(false)),
        PassthroughOptions::default(),
        &cancel,
    );
    tokio::pin!(second);
    assert!(
        tokio::time::timeout(Duration::from_millis(10), &mut second)
            .await
            .is_err()
    );
    assert_eq!(mock.requests().len(), 3);
    drop(first);
    let second = tokio::time::timeout(Duration::from_secs(1), second)
        .await
        .unwrap()
        .unwrap();
    drain(second).await;
    assert_eq!(mock.requests().len(), 4);
}

#[tokio::test]
async fn core_retry_policy_cannot_replay_model_request() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(response(503, json!({"error":{"code":"overloaded"}})));
    let (_, ep, _, _) = setup(mock.clone());
    let core = Core::builder(mock.clone())
        .retry(
            RetryPolicy {
                base_delay: Duration::ZERO,
                ..Default::default()
            },
            Arc::new(ncore_tokio::TokioClock),
        )
        .build();
    let result = core
        .passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, 503);
    assert_eq!(result.attempts, 1);
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        1
    );
}

#[tokio::test]
async fn transport_failure_is_not_retried() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.replies
        .lock()
        .unwrap()
        .push_back(Err(TransportError::new("disconnected")));
    let (core, ep, _, _) = setup(mock.clone());
    assert!(
        core.passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(mock.requests().len(), 3);
}

struct FailingStore {
    saved: Mutex<Credentials>,
}
impl CredentialStore for FailingStore {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>> {
        boxed(async { Ok(self.saved.lock().unwrap().clone()) })
    }
    fn save<'a>(&'a self, _: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async { Err("private filesystem error".into()) })
    }
}
#[tokio::test]
async fn persistence_failure_blocks_model_request() {
    let mock = Arc::new(Mock::default());
    mock.push(allowance('a'));
    let plugin = Arc::new(
        DeepAIAcc::new(
            Arc::new(FailingStore {
                saved: Mutex::new(credentials("test-key")),
            }),
            ClientInfo::default(),
        )
        .unwrap(),
    );
    let ep = DeepAIAcc::endpoint().with_plugin(plugin);
    let core = Core::builder(mock.clone()).build();
    let error = core
        .passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    assert!(!format!("{error:?}").contains("private filesystem error"));
    assert_eq!(mock.requests().len(), 1);
}

#[tokio::test]
async fn full_large_tool_result_is_not_compacted_or_truncated() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(answer(true));
    let (core, ep, _, _) = setup(mock.clone());
    let large = format!(
        "BEGIN:{}:END",
        "synthetic full tool content\n".repeat(40000)
    );
    let mut input = body(false);
    input["messages"][2]["content"] = large.clone().into();
    let result = core
        .passthrough(
            &ep,
            incoming(input),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    drain(result).await;
    let sent: Value = serde_json::from_slice(&mock.requests()[2].body).unwrap();
    assert_eq!(sent["messages"][2]["content"], large);
    assert_eq!(sent["messages"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn ir_generation_decodes_reasoning_and_text() {
    for streaming in [true, false] {
        let mock = Arc::new(Mock::default());
        mock.startup();
        mock.push(answer(true));
        let (core, ep, _, _) = setup(mock);
        let request = Request {
            model: "glm-5.3-flash".into(),
            messages: vec![Message::new(Role::User, vec![Block::text("Hi")])],
            max_tokens: Some(64),
            stream: streaming,
            ..Default::default()
        };
        let mut generation = core
            .generate(
                &ep,
                &request,
                &GenerateOptions {
                    encode: DeepAIAcc::encode_options(),
                    ..Default::default()
                },
                &CancelToken::new(),
            )
            .await
            .unwrap();
        let mut accumulator = Accumulator::new();
        while let Some(event) = generation.next().await {
            accumulator.push(&event.unwrap());
        }
        assert!(accumulator.is_done());
        let response = accumulator.finish();
        assert_eq!(response.stop, Some(StopReason::EndTurn));
        assert!(
            response
                .content
                .iter()
                .any(|b| matches!(b,Block::Thinking(t) if t.text=="Thought"))
        );
        assert_eq!(ncore::core::ir::plain_text(&response.content), "OK");
    }
}

#[tokio::test]
async fn responses_native_endpoint_collects_encrypted_reasoning() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    let reply = json!({"id":"resp-1","model":"muse-ember","status":"completed","output":[{"id":"rs-1","type":"reasoning","summary":[{"type":"summary_text","text":"Thought"}],"encrypted_content":"opaque-reasoning"},{"id":"msg-1","type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"OK","annotations":[]}]}],"usage":{"input_tokens":10,"output_tokens":4,"total_tokens":14}});
    mock.push(events(&[json!({"type":"response.created","response":{"id":"resp-1","model":"muse-ember","status":"in_progress","output":[]}}),json!({"type":"response.output_item.added","output_index":0,"item":reply["output"][0]}),json!({"type":"response.output_item.done","output_index":0,"item":reply["output"][0]}),json!({"type":"response.output_item.added","output_index":1,"item":reply["output"][1]}),json!({"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"OK"}),json!({"type":"response.output_item.done","output_index":1,"item":reply["output"][1]}),json!({"type":"response.completed","response":reply})],false));
    let (core, _, plugin, _) = setup(mock.clone());
    let ep = Session::from_ids("session-r", "thread-r")
        .unwrap()
        .attach(DeepAIAcc::responses_endpoint().with_plugin(plugin))
        .unwrap();
    let input=InboundRequest {method:"POST".into(),path_and_query:"/v1/responses".into(),headers:vec![],body:Bytes::from(json!({"model":"muse-ember","input":"Hi","stream":false,"max_output_tokens":64,"reasoning":{"effort":"max"}}).to_string())};
    let output = core
        .passthrough(
            &ep,
            input,
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let PassthroughBody::Full { bytes, .. } = output.body else {
        panic!("full response expected")
    };
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["output"], reply["output"]);
    assert_eq!(value["output"][1]["id"], "msg-1");
    assert_eq!(value["output"][1]["content"][0]["text"], "OK");
    assert_eq!(value["output"][0]["encrypted_content"], "opaque-reasoning");
    assert_eq!(value["usage"]["input_tokens"], 10);
    let requests = mock.requests();
    assert_eq!(
        requests[2].url,
        "https://api.deepai.org/deepai_code/responses"
    );
    let sent: Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(sent["store"], false);
    assert!(
        sent["include"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "reasoning.encrypted_content")
    );
}

struct ReadFixture {
    file: std::path::PathBuf,
    calls: Mutex<Vec<String>>,
}
impl ncore::Tools for ReadFixture {
    fn definitions(&self) -> Vec<ncore::core::ir::ToolDef> {
        vec![ncore::core::ir::ToolDef::Function(
            ncore::core::ir::Tool {
                name: "read_fixture".into(),
                description: None,
                schema: json!({"type":"object","properties":{},"additionalProperties":false}),
                strict: None,
                cache: None,
                ext: Default::default(),
            },
        )]
    }
    fn call<'a>(
        &'a self,
        call: &'a ncore::core::ir::ToolCall,
        _: &'a CancelToken,
    ) -> BoxFuture<'a, ncore::core::ir::ToolResult> {
        boxed(async move {
            self.calls.lock().unwrap().push(call.id.clone());
            let content = std::fs::read_to_string(&self.file).unwrap();
            ncore::core::ir::ToolResult {
                call_id: call.id.clone(),
                name: Some(call.name.clone()),
                content: vec![Block::text(content)],
                is_error: false,
                cache: None,
            }
        })
    }
}
impl Drop for ReadFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

#[tokio::test]
async fn agent_executes_interleaved_tools_and_replays_full_results_and_reasoning() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(events(&[
        json!({"id":"tools-1","model":"glm-5.3-flash","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"Read both.","reasoning_details":[{"index":0,"type":"reasoning.encrypted","format":"test","data":"signed-"}],"tool_calls":[{"index":0,"id":"t1","type":"function","function":{"name":"read_","arguments":"{"}},{"index":1,"id":"t2","type":"function","function":{"name":"read_","arguments":"{"}}]}}]}),
        json!({"choices":[{"index":0,"delta":{"reasoning_details":[{"index":0,"type":"reasoning.encrypted","data":"history"}],"tool_calls":[{"index":1,"function":{"name":"fixture","arguments":"}"}},{"index":0,"function":{"name":"fixture","arguments":"}"}}]}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}),
    ],true));
    mock.push(answer(true));
    let (core, ep, _, _) = setup(mock.clone());
    let file = std::env::temp_dir().join(format!(
        "deepaiacc-test-{}.txt",
        Session::new().unwrap().cache_key
    ));
    let content = format!("HEAD:{}:TAIL", "complete local read result\n".repeat(20000));
    std::fs::write(&file, &content).unwrap();
    let tools = ReadFixture {
        file,
        calls: Mutex::default(),
    };
    let agent =
        ncore::Agent::new(&core, &ep, Arc::new(ncore_tokio::TokioClock)).tools(&tools);
    let request = Request {
        model: "glm-5.3-flash".into(),
        messages: vec![Message::new(
            Role::User,
            vec![Block::text("Read both fixtures")],
        )],
        max_tokens: Some(64),
        stream: true,
        ..Default::default()
    };
    let output = agent
        .run(
            request,
            ncore::RunOptions {
                max_steps: 2,
                restart_streams: false,
                generate: GenerateOptions {
                    encode: DeepAIAcc::encode_options(),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .complete()
        .await;
    assert!(matches!(
        output.stop,
        ncore::RunStop::Finished(Some(StopReason::EndTurn))
    ));
    assert_eq!(output.steps, 2);
    assert_eq!(tools.calls.lock().unwrap().len(), 2);
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    let next: Value = serde_json::from_slice(&requests[3].body).unwrap();
    let messages = next["messages"].as_array().unwrap();
    let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["reasoning_content"], "Read both.");
    assert_eq!(assistant["reasoning_details"][0]["data"], "signed-history");
    assert_eq!(assistant["tool_calls"].as_array().unwrap().len(), 2);
    let results = messages
        .iter()
        .filter(|m| m["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(results.len(), 2);
    for result in results {
        assert_eq!(result["content"], content);
    }
    assert_eq!(next["prompt_cache_key"], "test-thread");
}

#[tokio::test]
async fn model_errors_keep_safe_diagnostics_when_the_provider_code_is_missing_or_unknown() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(response(
        429,
        json!({"error":{"type":"rate_limit_error","code":null,"message":"private text"}}),
    ));
    mock.push(response(
        429,
        json!({"error":{"type":"overloaded_error","code":"private code","message":"private text"}}),
    ));
    mock.push(response(
        429,
        json!({"error":{"code":"private code","message":"private text"}}),
    ));
    mock.push(HttpResponse {
        status: 502,
        headers: vec![],
        body: Box::pin(stream::iter([Ok(Bytes::from_static(
            b"private proxy text",
        ))])),
    });
    let (core, ep, _, _) = setup(mock.clone());
    let request = Request {
        model: "glm-5.3-flash".into(),
        messages: vec![Message::new(Role::User, vec![Block::text("Hi")])],
        max_tokens: Some(64),
        stream: true,
        ..Default::default()
    };
    for (status, kind, code, message) in [
        (
            429,
            ErrorKind::RateLimited,
            "rate_limit_error",
            "DeepAI rate limit reached.",
        ),
        (
            429,
            ErrorKind::Overloaded,
            "overloaded_error",
            "DeepAI is overloaded.",
        ),
        (
            429,
            ErrorKind::RateLimited,
            "deepai_error",
            "DeepAI request failed (HTTP 429).",
        ),
        (
            502,
            ErrorKind::Server,
            "deepai_error",
            "DeepAI request failed (HTTP 502).",
        ),
    ] {
        let error = core
            .generate(
                &ep,
                &request,
                &GenerateOptions::default(),
                &CancelToken::new(),
            )
            .await
            .unwrap_err();
        let ncore::Error::Provider(error) = error else {
            panic!("expected a provider error");
        };
        assert_eq!(error.status, Some(status));
        assert_eq!(error.kind, kind);
        assert_eq!(error.code.as_deref(), Some(code));
        assert_eq!(error.message, message);
        assert!(!String::from_utf8_lossy(&error.raw).contains("private"));
    }
    assert_eq!(mock.requests().len(), 6);
}

#[tokio::test]
async fn streamed_error_and_redirect_never_become_success() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(events(
        &[json!({"type":"error","error":{"code":null,"type":"overloaded_error","message":"private text"}})],
        false,
    ));
    mock.push(response(302, json!({"location":"https://example.com"})));
    let (core, ep, _, _) = setup(mock.clone());
    let request = Request {
        model: "glm-5.3-flash".into(),
        messages: vec![Message::new(Role::User, vec![Block::text("Hi")])],
        max_tokens: Some(64),
        stream: false,
        ..Default::default()
    };
    let error = core
        .generate(
            &ep,
            &request,
            &GenerateOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap_err();
    let ncore::Error::Provider(error) = error else {
        panic!("expected a provider error");
    };
    assert_eq!(error.kind, ErrorKind::Overloaded);
    assert_eq!(error.status, None);
    assert!(!String::from_utf8_lossy(&error.raw).contains("private text"));
    assert!(
        core.generate(
            &ep,
            &request,
            &GenerateOptions::default(),
            &CancelToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(mock.requests().len(), 4);
}

#[tokio::test]
async fn native_endpoint_mismatch_never_calls_model() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    let (core, ep, _, _) = setup(mock.clone());
    let mut input = body(false);
    input["model"] = "muse-ember".into();
    assert!(
        core.passthrough(
            &ep,
            incoming(input),
            PassthroughOptions::default(),
            &CancelToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test]
async fn default_agent_restart_cannot_replay_deepai_model() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    let partial = Bytes::from(format!(
        "data: {}\n\n",
        json!({"id":"partial-1","model":"glm-5.3-flash","choices":[{"index":0,"delta":{"content":"partial reply"}}]})
    ));
    mock.push(HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: Box::pin(stream::iter([
            Ok(partial),
            Err(TransportError::new("stream disconnected")),
        ])),
    });
    let (_, ep, _, _) = setup(mock.clone());
    let core = Core::builder(mock.clone())
        .retry(
            RetryPolicy {
                base_delay: Duration::ZERO,
                ..Default::default()
            },
            Arc::new(ncore_tokio::TokioClock),
        )
        .build();
    let agent = ncore::Agent::new(&core, &ep, Arc::new(ncore_tokio::TokioClock));
    let request = Request {
        model: "glm-5.3-flash".into(),
        messages: vec![Message::new(Role::User, vec![Block::text("Hi")])],
        max_tokens: Some(64),
        stream: true,
        ..Default::default()
    };
    let output = agent
        .run(request, ncore::RunOptions::default())
        .complete()
        .await;
    assert!(matches!(output.stop, ncore::RunStop::Failed(_)));
    assert_eq!(output.steps, 1);
    assert_eq!(
        ncore::core::ir::plain_text(&output.partial.unwrap().content),
        "partial reply"
    );
    assert_eq!(
        mock.requests()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        1
    );
    assert!(!ep.allow_replays);
    assert!(!DeepAIAcc::responses_endpoint().allow_replays);
}

#[tokio::test]
async fn expired_nonce_without_fresh_mark_blocks_model() {
    let mock = Arc::new(Mock::default());
    mock.push(response(
        200,
        json!({"enabled":true,"balance":"78","granted":"78"}),
    ));
    let (core, ep, _, store) = setup(mock.clone());
    let mut saved = store.load().await.unwrap();
    saved.check_in = Some(ncore_deepaiacc::CheckIn {
        nonce: "a".repeat(32),
        seconds: 600.0,
        checked_at: 1,
    });
    store.save(&saved).await.unwrap();
    assert!(
        core.passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(mock.requests().len(), 1);
    assert_eq!(mock.requests()[0].method, "GET");
}

#[tokio::test]
async fn done_only_stream_is_not_a_valid_completion() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(events(&[], true));
    let (core, ep, _, _) = setup(mock.clone());
    assert!(
        core.passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new()
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn all_output_limit_aliases_become_effective_chat_limit() {
    for field in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        let mock = Arc::new(Mock::default());
        mock.startup();
        mock.push(answer(true));
        let (core, ep, _, _) = setup(mock.clone());
        let mut input = body(false);
        input
            .as_object_mut()
            .unwrap()
            .remove("max_completion_tokens");
        input[field] = 64.into();
        drain(
            core.passthrough(
                &ep,
                incoming(input),
                PassthroughOptions::default(),
                &CancelToken::new(),
            )
            .await
            .unwrap(),
        )
        .await;
        let sent: Value = serde_json::from_slice(&mock.requests()[2].body).unwrap();
        assert_eq!(sent["max_completion_tokens"], 64);
        assert!(sent.get("max_tokens").is_none());
        assert!(sent.get("max_output_tokens").is_none());
    }
}

#[tokio::test]
async fn conflicting_output_aliases_never_call_model() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    let (core, ep, _, _) = setup(mock.clone());
    let mut input = body(false);
    input["max_output_tokens"] = 65.into();
    assert!(
        core.passthrough(
            &ep,
            incoming(input),
            PassthroughOptions::default(),
            &CancelToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(mock.requests().len(), 2);
}

#[tokio::test]
async fn responses_terminal_snapshot_keeps_output_annotations_and_metadata() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    let reply = json!({"id":"resp-final","model":"muse-ember","status":"completed","metadata":{"probe":"retained"},"output":[{"id":"msg-final","type":"message","role":"assistant","content":[{"type":"output_text","text":"FINAL","annotations":[{"type":"url_citation","url":"https://example.org/","title":"fixture"}]}]}],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}});
    let mut response = events(
        &[json!({"type":"response.completed","response":reply})],
        false,
    );
    response.body = Box::pin(
        stream::iter([Ok(Bytes::from(": keepalive\n\n"))])
            .chain(response.body)
            .chain(stream::iter([Ok(Bytes::from(": keepalive\n\n"))])),
    );
    mock.push(response);
    let (core, _, plugin, _) = setup(mock.clone());
    let ep = DeepAIAcc::responses_endpoint().with_plugin(plugin);
    let input = InboundRequest {
        method: "POST".into(),
        path_and_query: "/v1/responses".into(),
        headers: vec![],
        body: Bytes::from(
            json!({"model":"muse-ember","input":"Hi","stream":false,"max_tokens":64}).to_string(),
        ),
    };
    let output = core
        .passthrough(
            &ep,
            input,
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let PassthroughBody::Full { bytes, .. } = output.body else {
        panic!("full response expected")
    };
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value, reply);
    let sent: Value = serde_json::from_slice(&mock.requests()[2].body).unwrap();
    assert_eq!(sent["max_output_tokens"], 64);
    assert!(sent.get("max_tokens").is_none());
}

#[tokio::test]
async fn cancelled_stream_retained_by_caller_releases_account_slot() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(answer(true));
    mock.push(answer(true));
    let (core, ep, _, _) = setup(mock.clone());
    let cancel = CancelToken::new();
    let first = core
        .passthrough(
            &ep,
            incoming(body(true)),
            PassthroughOptions::default(),
            &cancel,
        )
        .await
        .unwrap();
    let PassthroughBody::Stream(mut first) = first.body else {
        panic!("stream expected")
    };
    cancel.cancel();
    assert!(matches!(
        first.next().await,
        Some(Err(ncore::Error::Cancelled))
    ));
    assert!(first.next().await.is_none());
    let second = tokio::time::timeout(
        Duration::from_secs(1),
        core.passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    drain(second).await;
    assert_eq!(mock.requests().len(), 4);
    drop(first);
}

#[tokio::test]
async fn failed_stream_retained_by_caller_releases_account_slot() {
    let mock = Arc::new(Mock::default());
    mock.startup();
    mock.push(HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: Box::pin(stream::iter([Err(TransportError::new("broken body"))])),
    });
    mock.push(answer(true));
    let (core, ep, _, _) = setup(mock.clone());
    let first = core
        .passthrough(
            &ep,
            incoming(body(true)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let PassthroughBody::Stream(mut first) = first.body else {
        panic!("stream expected")
    };
    assert!(first.next().await.unwrap().is_err());
    assert!(first.next().await.is_none());
    let second = tokio::time::timeout(
        Duration::from_secs(1),
        core.passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    drain(second).await;
    assert_eq!(mock.requests().len(), 4);
    drop(first);
}

#[test]
fn invalid_client_version_is_rejected_locally() {
    assert!(
        DeepAIAcc::new(
            Arc::new(MemoryStore::new(credentials("test-key"))),
            ClientInfo::new("0.3.37\r\nsecret")
        )
        .is_err()
    );
}

#[tokio::test]
async fn direct_hook_keeps_stable_cache_key_and_strips_local_headers() {
    let mock = Arc::new(Mock::default());
    let (_, _, plugin, _) = setup(mock);
    let ctx = RequestCtx {
        id: 1,
        format: Format::OpenAiChat,
        streaming: true,
        attempt: 0,
    };
    let mut req = HttpRequest {
        method: "POST".into(),
        url: "https://api.deepai.org/deepai_code/v1/chat/completions".into(),
        headers: vec![
            ("x-session-affinity".into(), "session-a".into()),
            ("x-deepai-cache-key".into(), "thread-a".into()),
        ],
        body: Bytes::from(body(true).to_string()),
    };
    plugin.on_request(&ctx, &mut req).await.unwrap();
    assert_eq!(req.headers.len(), 1);
    assert_eq!(
        serde_json::from_slice::<Value>(&req.body).unwrap()["prompt_cache_key"],
        "thread-a"
    );
    plugin.on_request(&ctx, &mut req).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&req.body).unwrap()["prompt_cache_key"],
        "thread-a"
    );
}
