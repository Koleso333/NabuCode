use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use ncore::core::ir::{
    Accumulator, Block, Message, Request, Role, StopReason, Tool, ToolCall, ToolDef, ToolResult,
};
use ncore::rt::{BoxFuture, boxed, header};
use ncore::{
    Agent, CancelToken, Core, Error, GenerateOptions, HttpRequest, HttpResponse, InboundRequest,
    LoopEvent, PassthroughBody, PassthroughOptions, RunOptions, RunStop, Tools, Transport,
    TransportError,
};
use ncore_chatgptsub::{ChatGptSub, CredentialStore, Credentials, MemoryStore, oauth, request};
use ncore_tokio::TokioClock;
use serde_json::{Value, json};

fn credentials(expired: bool) -> Credentials {
    Credentials {
        access_token: "old-secret".into(),
        refresh_token: Some("refresh-secret".into()),
        expires_at: if expired { 0 } else { u64::MAX },
        account_id: "account-1".into(),
    }
}

fn json_response(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: Box::pin(stream::iter([Ok(Bytes::from(value.to_string()))])),
    }
}

fn sse_response(text: String, hang: bool) -> HttpResponse {
    let chunks = text
        .as_bytes()
        .chunks(3)
        .map(|b| Ok(Bytes::copy_from_slice(b)))
        .collect::<Vec<_>>();
    let tail = if hang {
        Box::pin(stream::pending()) as ncore::rt::BoxStream<'static, _>
    } else {
        Box::pin(stream::empty())
    };
    HttpResponse {
        status: 200,
        headers: vec![
            (
                "content-type".into(),
                "text/event-stream; charset=utf-8".into(),
            ),
            ("x-codex-primary-used-percent".into(), "13".into()),
        ],
        body: Box::pin(stream::iter(chunks).chain(tail)),
    }
}

fn event(value: Value) -> String {
    format!("data: {value}\r\n\r\n")
}

fn response(output: Vec<Value>) -> Value {
    json!({"id":"resp_1", "object":"response", "model":"fixture", "status":"completed", "output":output,
        "usage":{"input_tokens":10,"output_tokens":3,"input_tokens_details":{"cached_tokens":7}}})
}

fn output_message() -> Value {
    json!({"type":"message","id":"msg_1","role":"assistant","status":"completed",
        "content":[{"type":"output_text","text":"Привет","annotations":[]}]})
}

fn turn(output: Vec<Value>) -> String {
    let mut s = String::new();
    for (i, item) in output.iter().enumerate() {
        let mut start = item.clone();
        if item["type"] == "function_call" {
            start["arguments"] = json!("");
        }
        s += &event(json!({"type":"response.output_item.added", "output_index":i, "item":start}));
        if item["type"] == "message" {
            s += &event(
                json!({"type":"response.content_part.added","output_index":i,"content_index":0,"part":{"type":"output_text","text":""}}),
            );
            s += &event(
                json!({"type":"response.output_text.delta","output_index":i,"content_index":0,"delta":item["content"][0]["text"]}),
            );
            s += &event(
                json!({"type":"response.content_part.done","output_index":i,"content_index":0}),
            );
        }
        if item["type"] == "function_call" {
            s += &event(
                json!({"type":"response.function_call_arguments.delta","output_index":i,"delta":item["arguments"]}),
            );
        }
        s += &event(json!({"type":"response.output_item.done","output_index":i,"item":item}));
    }
    s + &event(json!({"type":"response.completed", "response":response(output)}))
}

#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<HttpRequest>>,
    replies: Mutex<VecDeque<HttpResponse>>,
    refreshes: AtomicUsize,
    reject_old: bool,
    reject_all: bool,
    refresh_status: u16,
}

impl Transport for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.seen.lock().unwrap().push(request.clone());
        boxed(async move {
            if request.url == oauth::TOKEN_URL {
                self.refreshes.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                if self.refresh_status >= 400 {
                    return Ok(json_response(
                        self.refresh_status,
                        json!({"error":"invalid_grant"}),
                    ));
                }
                return Ok(json_response(
                    200,
                    json!({"access_token":"fresh-secret", "refresh_token":"rotated-secret", "expires_in":3600}),
                ));
            }
            if self.reject_all
                || (self.reject_old
                    && header(&request.headers, "authorization") == Some("Bearer old-secret"))
            {
                return Ok(json_response(
                    401,
                    json!({"error":{"message":"rejected","type":"authentication_error"}}),
                ));
            }
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| sse_response(turn(vec![output_message()]), true)))
        })
    }
}

fn setup(
    mock: Arc<Mock>,
    store: Arc<dyn CredentialStore>,
) -> (Core, ncore::Endpoint, Arc<ChatGptSub>) {
    let plugin = Arc::new(ChatGptSub::new(store));
    (
        Core::builder(mock).build(),
        ChatGptSub::endpoint().with_plugin(plugin.clone()),
        plugin,
    )
}

fn req(stream: bool) -> Request {
    Request {
        model: "fixture".into(),
        system: vec![Block::text("Host instructions")],
        messages: vec![Message::new(Role::User, vec![Block::text("Hello")])],
        stream,
        max_tokens: Some(32),
        ..Default::default()
    }
}

async fn collect(
    core: &Core,
    endpoint: &ncore::Endpoint,
    request: &Request,
) -> Result<ncore::core::ir::Response, Error> {
    let future = async {
        let mut generation = core
            .generate(
                endpoint,
                request,
                &GenerateOptions::default(),
                &CancelToken::new(),
            )
            .await?;
        let mut acc = Accumulator::new();
        while let Some(event) = generation.next().await {
            acc.push(&event?);
        }
        Ok(generation.take_response().unwrap_or_else(|| acc.finish()))
    };
    tokio::time::timeout(Duration::from_secs(2), future)
        .await
        .expect("stream waited for HTTP EOF")
}

#[test]
fn preparation_preserves_host_data_and_is_idempotent() {
    let body = json!({"instructions":"Custom host system", "input":[
        {"role":"developer","content":"Keep this"},
        {"type":"reasoning","encrypted_content":"opaque"},
        {"type":"function_call","call_id":"c1","name":"echo","arguments":"{\"value\":1}"}],
        "tools":[{"type":"function","name":"echo","parameters":{"type":"object","additionalProperties":false}}],
        "reasoning":{"effort":"high"},"prompt_cache_key":"session-1", "include":["future.field"],
        "max_output_tokens":32,"background":true,"store":true,"stream":false,"future":{"a":1}});
    let encoded = request::prepare(body.to_string().as_bytes()).unwrap();
    let out: Value = serde_json::from_slice(&encoded).unwrap();
    for name in [
        "instructions",
        "input",
        "tools",
        "reasoning",
        "prompt_cache_key",
        "future",
    ] {
        assert_eq!(out[name], body[name]);
    }
    assert_eq!(out["store"], false);
    assert_eq!(out["stream"], true);
    assert!(out.get("max_output_tokens").is_none());
    assert!(out.get("background").is_none());
    assert_eq!(
        out["include"],
        json!(["future.field", "reasoning.encrypted_content"])
    );
    assert_eq!(request::prepare(&encoded).unwrap(), encoded);
    let string_input: Value =
        serde_json::from_slice(&request::prepare(br#"{"input":"hello"}"#).unwrap()).unwrap();
    assert_eq!(string_input["input"][0]["role"], "user");
    let system_input: Value = serde_json::from_slice(
        &request::prepare(br#"{"input":[{"role":"system","content":"host"}]}"#).unwrap(),
    )
    .unwrap();
    assert_eq!(
        system_input["input"][0],
        json!({"role":"developer","content":"host"})
    );
    for invalid in [
        "[]",
        "broken",
        r#"{"instructions":{}}"#,
        r#"{"include":false}"#,
        r#"{"input":null}"#,
    ] {
        assert!(request::prepare(invalid.as_bytes()).is_err());
    }
}

#[tokio::test]
async fn translation_uses_existing_codecs_in_all_formats_and_modes() {
    use ncore::{Format, TranslateBody, TranslateOptions};
    for client in [
        Format::AnthropicMessages,
        Format::OpenAiChat,
        Format::Gemini,
    ] {
        for streaming in [false, true] {
            let mock = Arc::new(Mock::default());
            let (core, endpoint, _) = setup(mock, Arc::new(MemoryStore::new(credentials(false))));
            let (path, body) = match client {
                Format::AnthropicMessages => (
                    "/v1/messages".to_owned(),
                    json!({"model":"fixture","system":"Host instructions","messages":[{"role":"user","content":"Hi"}],"max_tokens":32,"stream":streaming}),
                ),
                Format::OpenAiChat => (
                    "/v1/chat/completions".to_owned(),
                    json!({"model":"fixture","messages":[{"role":"system","content":"Host instructions"},{"role":"user","content":"Hi"}],"stream":streaming}),
                ),
                Format::Gemini => (
                    format!(
                        "/v1beta/models/fixture:{}",
                        if streaming {
                            "streamGenerateContent?alt=sse"
                        } else {
                            "generateContent"
                        }
                    ),
                    json!({"contents":[{"role":"user","parts":[{"text":"Hi"}]}]}),
                ),
                _ => unreachable!(),
            };
            let incoming = InboundRequest {
                method: "POST".into(),
                path_and_query: path,
                headers: vec![],
                body: Bytes::from(body.to_string()),
            };
            tokio::time::timeout(Duration::from_secs(2), async {
                let result = core
                    .translate(
                        &endpoint,
                        incoming,
                        TranslateOptions {
                            strict_stream: true,
                            ..Default::default()
                        },
                        &CancelToken::new(),
                    )
                    .await
                    .unwrap();
                assert_eq!(result.status, 200);
                match result.body {
                    TranslateBody::Full { response, .. } => {
                        assert_eq!(response.unwrap().content, vec![Block::text("Привет")])
                    }
                    TranslateBody::Stream(mut stream) => {
                        let mut bytes = Vec::new();
                        while let Some(chunk) = stream.next().await {
                            bytes.extend_from_slice(&chunk.unwrap());
                        }
                        assert!(String::from_utf8(bytes).unwrap().contains("Привет"));
                    }
                }
            })
            .await
            .unwrap();
        }
    }
}

#[tokio::test]
async fn cancellation_interrupts_streaming_and_buffered_inference() {
    for streaming in [true, false] {
        let mock = Arc::new(Mock::default());
        mock.replies
            .lock()
            .unwrap()
            .push_back(sse_response(String::new(), true));
        let (core, endpoint, _) = setup(mock, Arc::new(MemoryStore::new(credentials(false))));
        let cancel = CancelToken::new();
        let request = req(streaming);
        let options = GenerateOptions::default();
        let future = async {
            let mut generation = core
                .generate(&endpoint, &request, &options, &cancel)
                .await?;
            while let Some(event) = generation.next().await {
                event?;
            }
            Ok::<(), Error>(())
        };
        tokio::pin!(future);
        tokio::select! {
            _ = tokio::task::yield_now() => cancel.cancel(),
            _ = &mut future => panic!("inference must wait"),
        }
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), future)
                .await
                .unwrap(),
            Err(Error::Cancelled)
        ));
    }
}

#[tokio::test]
async fn oversized_events_and_wrong_content_type_fail_without_hanging() {
    for streaming in [false, true] {
        let oversized = HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: Box::pin(stream::iter([Ok(Bytes::from(format!(
                "data: {}\n\n",
                "x".repeat(8 * 1024 * 1024 + 1)
            )))])),
        };
        for reply in [oversized, json_response(200, json!({"unexpected":"json"}))] {
            let mock = Arc::new(Mock::default());
            mock.replies.lock().unwrap().push_back(reply);
            let (core, endpoint, _) = setup(mock, Arc::new(MemoryStore::new(credentials(false))));
            assert!(collect(&core, &endpoint, &req(streaming)).await.is_err());
        }
    }
}

#[tokio::test]
async fn generate_supports_both_modes_without_waiting_for_eof() {
    let mock = Arc::new(Mock::default());
    let (core, endpoint, plugin) =
        setup(mock.clone(), Arc::new(MemoryStore::new(credentials(false))));
    for streaming in [false, true] {
        let out = collect(&core, &endpoint, &req(streaming)).await.unwrap();
        assert_eq!(out.stop, Some(StopReason::EndTurn));
        assert_eq!(out.content, vec![Block::text("Привет")]);
        assert_eq!(out.usage.input_total, Some(10));
        assert_eq!(out.usage.cache_read, Some(7));
    }
    assert_eq!(plugin.last_limits()[0].1, "13");
    for sent in mock.seen.lock().unwrap().iter() {
        assert_eq!(sent.url, "https://chatgpt.com/backend-api/codex/responses");
        assert_eq!(
            header(&sent.headers, "authorization"),
            Some("Bearer old-secret")
        );
        let body: Value = serde_json::from_slice(&sent.body).unwrap();
        assert_eq!(body["instructions"], "Host instructions");
        assert_eq!(body["stream"], true);
    }
}

#[tokio::test]
async fn codex_sse_without_content_type_works_in_both_modes() {
    for streaming in [true, false] {
        let mock = Arc::new(Mock::default());
        let mut reply = sse_response(turn(vec![output_message()]), true);
        reply.headers.retain(|(key, _)| key != "content-type");
        mock.replies.lock().unwrap().push_back(reply);
        let (core, endpoint, _) = setup(mock, Arc::new(MemoryStore::new(credentials(false))));
        let response = collect(&core, &endpoint, &req(streaming)).await.unwrap();
        assert_eq!(response.content, vec![Block::text("Привет")]);
    }
}

#[tokio::test]
async fn expired_and_rejected_credentials_refresh_once_across_concurrent_requests() {
    for expired in [true, false] {
        let mock = Arc::new(Mock {
            reject_old: !expired,
            ..Default::default()
        });
        let store = Arc::new(MemoryStore::new(credentials(expired)));
        let (core, endpoint, _) = setup(mock.clone(), store.clone());
        let request = req(true);
        for out in
            futures_util::future::join_all((0..8).map(|_| collect(&core, &endpoint, &request)))
                .await
        {
            out.unwrap();
        }
        assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.load().await.unwrap().refresh_token.as_deref(),
            Some("rotated-secret")
        );
        let seen = mock.seen.lock().unwrap();
        let refresh = seen.iter().find(|r| r.url == oauth::TOKEN_URL).unwrap();
        let form = std::str::from_utf8(&refresh.body).unwrap();
        assert!(form.contains("grant_type=refresh_token"));
        assert!(!form.contains("scope="));
    }
}

#[tokio::test]
async fn second_401_is_not_replayed_and_refresh_failures_cool_down() {
    let mock = Arc::new(Mock {
        reject_all: true,
        ..Default::default()
    });
    let (core, endpoint, _) = setup(mock.clone(), Arc::new(MemoryStore::new(credentials(false))));
    assert!(matches!(
        collect(&core, &endpoint, &req(true)).await,
        Err(Error::Provider(_))
    ));
    assert_eq!(mock.seen.lock().unwrap().len(), 3);
    let mock = Arc::new(Mock {
        refresh_status: 401,
        ..Default::default()
    });
    let (core, endpoint, _) = setup(mock.clone(), Arc::new(MemoryStore::new(credentials(true))));
    for _ in 0..2 {
        assert!(collect(&core, &endpoint, &req(true)).await.is_err());
    }
    assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1);
}

struct InterruptibleStore {
    inner: MemoryStore,
    saves: AtomicUsize,
    started: tokio::sync::Notify,
    fail: bool,
}

impl CredentialStore for InterruptibleStore {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>> {
        self.inner.load()
    }
    fn save<'a>(&'a self, value: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            if self.saves.fetch_add(1, Ordering::SeqCst) == 0 {
                self.started.notify_one();
                if self.fail {
                    return Err("storage failed: private detail".into());
                }
                return std::future::pending().await;
            }
            self.inner.save(value).await
        })
    }
}

#[tokio::test]
async fn rotated_credentials_survive_failed_or_cancelled_save() {
    for fail in [true, false] {
        let mock = Arc::new(Mock::default());
        let store = Arc::new(InterruptibleStore {
            inner: MemoryStore::new(credentials(true)),
            saves: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            fail,
        });
        let (core, endpoint, _) = setup(mock.clone(), store.clone());
        if fail {
            let error = collect(&core, &endpoint, &req(true)).await.unwrap_err();
            assert!(!error.to_string().contains("private detail"));
        } else {
            let request = req(true);
            let options = GenerateOptions::default();
            let cancel = CancelToken::new();
            let future = core.generate(&endpoint, &request, &options, &cancel);
            tokio::pin!(future);
            tokio::select! { _ = store.started.notified() => {}, _ = &mut future => panic!("save must wait") }
        }
        collect(&core, &endpoint, &req(true)).await.unwrap();
        assert_eq!(mock.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(store.load().await.unwrap().access_token, "fresh-secret");
    }
}

#[tokio::test]
async fn passthrough_replaces_credentials_and_keeps_session_affinity() {
    for streaming in [false, true] {
        let mock = Arc::new(Mock::default());
        let (core, endpoint, _) =
            setup(mock.clone(), Arc::new(MemoryStore::new(credentials(false))));
        let incoming = InboundRequest { method:"POST".into(), path_and_query:"/v1/responses".into(),
            headers:vec![("authorization".into(), "Bearer attacker".into()), ("cookie".into(), "secret".into()),
                ("chatgpt-account-id".into(), "wrong".into()), ("session_id".into(),"wrong".into())],
            body:Bytes::from(json!({"model":"fixture","input":"hello","stream":streaming,"prompt_cache_key":"session-1"}).to_string()) };
        let mut result = tokio::time::timeout(
            Duration::from_secs(2),
            core.passthrough(
                &endpoint,
                incoming,
                PassthroughOptions::default(),
                &CancelToken::new(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        match &mut result.body {
            PassthroughBody::Full { bytes, .. } => {
                assert_eq!(
                    serde_json::from_slice::<Value>(bytes).unwrap()["status"],
                    "completed"
                );
            }
            PassthroughBody::Stream(stream) => {
                tokio::time::timeout(Duration::from_secs(2), async {
                    while let Some(chunk) = stream.next().await {
                        chunk.unwrap();
                    }
                })
                .await
                .unwrap();
            }
        }
        let seen = mock.seen.lock().unwrap();
        assert_eq!(
            header(&seen[0].headers, "chatgpt-account-id"),
            Some("account-1")
        );
        assert_eq!(header(&seen[0].headers, "session_id"), Some("session-1"));
        assert!(header(&seen[0].headers, "cookie").is_none());
    }
}

#[tokio::test]
async fn unsupported_routes_and_invalid_credentials_never_reach_transport() {
    let mock = Arc::new(Mock::default());
    let (core, endpoint, _) = setup(mock.clone(), Arc::new(MemoryStore::new(credentials(false))));
    for path in [
        "/v1/responses/compact",
        "/v1/models",
        "/v1/responses?redirect=evil",
    ] {
        let request = InboundRequest {
            method: "POST".into(),
            path_and_query: path.into(),
            headers: vec![],
            body: Bytes::from_static(b"{}"),
        };
        assert!(
            core.passthrough(
                &endpoint,
                request,
                PassthroughOptions::default(),
                &CancelToken::new()
            )
            .await
            .is_err()
        );
    }
    let mut invalid = credentials(false);
    invalid.account_id = "injected\r\nheader".into();
    let (core, endpoint, _) = setup(mock.clone(), Arc::new(MemoryStore::new(invalid)));
    assert!(collect(&core, &endpoint, &req(true)).await.is_err());
    assert!(mock.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn failed_and_truncated_streams_are_errors_in_both_modes() {
    for streaming in [false, true] {
        for content in [
            event(
                json!({"type":"response.failed","response":{"status":"failed","error":{"type":"usage_limit_reached","message":"quota exhausted"}}}),
            ),
            event(json!({"type":"response.created","response":{"id":"r"}})),
            "data: {\"type\":\"response.completed\"}".into(),
        ] {
            let mock = Arc::new(Mock::default());
            mock.replies
                .lock()
                .unwrap()
                .push_back(sse_response(content, false));
            let (core, endpoint, _) = setup(mock, Arc::new(MemoryStore::new(credentials(false))));
            assert!(collect(&core, &endpoint, &req(streaming)).await.is_err());
        }
    }
}

#[tokio::test]
async fn monitoring_is_authenticated_and_usage_is_coalesced() {
    let mock = Arc::new(Mock::default());
    mock.replies.lock().unwrap().extend([
        json_response(200, json!({"plan_type":"plus","rate_limit":{"primary_window":{"used_percent":13},"secondary_window":null},"future":{"unknown":true}})),
        json_response(200, json!({"models":[{"slug":"fixture","future":1}]})),
    ]);
    let plugin = ChatGptSub::new(Arc::new(MemoryStore::new(credentials(false))));
    for result in futures_util::future::join_all((0..6).map(|_| plugin.usage(&*mock))).await {
        let snapshot = result.unwrap();
        assert!(!snapshot.stale);
        assert_eq!(snapshot.value["future"]["unknown"], true);
        assert!(snapshot.value["rate_limit"]["secondary_window"].is_null());
    }
    assert_eq!(
        plugin.models(&*mock, "0.0.1 test").await.unwrap()["models"][0]["slug"],
        "fixture"
    );
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(seen[1].url.ends_with("client_version=0.0.1%20test"));
    for request in seen.iter() {
        assert_eq!(request.method, "GET");
        assert_eq!(
            header(&request.headers, "chatgpt-account-id"),
            Some("account-1")
        );
    }
}

fn jwt(value: Value) -> String {
    format!("e30.{}.fixture", URL_SAFE_NO_PAD.encode(value.to_string()))
}

#[test]
fn import_reads_only_supplied_oauth_data_and_debug_redacts_secrets() {
    let token =
        jwt(json!({"exp":1234,"https://api.openai.com/auth":{"chatgpt_account_id":"account-1"}}));
    let raw = json!({"tokens":{"access_token":token,"refresh_token":"private-refresh"}});
    let imported = Credentials::from_codex_json(raw.to_string().as_bytes()).unwrap();
    assert_eq!(imported.expires_at, 1_234_000);
    assert_eq!(imported.account_id, "account-1");
    let debug = format!("{imported:?}");
    assert!(!debug.contains(&token));
    assert!(!debug.contains("private-refresh"));
    assert!(Credentials::from_codex_json(br#"{"OPENAI_API_KEY":"private"}"#).is_err());
}

struct LoginMock(Mutex<Vec<HttpRequest>>);
impl Transport for LoginMock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.0.lock().unwrap().push(request);
        boxed(async {
            Ok(json_response(
                200,
                json!({"access_token":jwt(json!({"https://api.openai.com/auth":{"chatgpt_account_id":"account-1"}})),
            "refresh_token":"refresh","expires_in":3600}),
            ))
        })
    }
}

#[tokio::test]
async fn oauth_checks_state_and_sends_pkce_form_without_secrets_in_debug() {
    let mock = LoginMock(Mutex::default());
    let start = oauth::start_login().unwrap();
    assert!(!format!("{start:?}").contains("state="));
    assert!(
        oauth::finish_login(&mock, start, "code", "wrong")
            .await
            .is_err()
    );
    assert!(mock.0.lock().unwrap().is_empty());
    let start = oauth::start_login().unwrap();
    let query: std::collections::HashMap<_, _> = start
        .url
        .split_once('?')
        .unwrap()
        .1
        .split('&')
        .map(|s| s.split_once('=').unwrap())
        .collect();
    let state = query["state"].to_owned();
    let challenge = query["code_challenge"].to_owned();
    assert_eq!(query["code_challenge_method"], "S256");
    let result = oauth::finish_login(&mock, start, "code +&", &state)
        .await
        .unwrap();
    assert_eq!(result.account_id, "account-1");
    let seen = mock.0.lock().unwrap();
    let form = std::str::from_utf8(&seen[0].body).unwrap();
    assert!(form.contains("code=code%20%2B%26"));
    let verifier = form
        .split('&')
        .find_map(|s| s.strip_prefix("code_verifier="))
        .unwrap();
    use sha2::{Digest, Sha256};
    assert_eq!(
        URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        challenge
    );
}

struct Echo(AtomicUsize);
impl Tools for Echo {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef::Function(Tool {
            name: "echo".into(),
            description: None,
            schema: json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
            strict: None,
            cache: None,
            ext: Default::default(),
        })]
    }
    fn call<'a>(&'a self, call: &'a ToolCall, _: &'a CancelToken) -> BoxFuture<'a, ToolResult> {
        self.0.fetch_add(1, Ordering::SeqCst);
        boxed(async move {
            ToolResult {
                call_id: call.id.clone(),
                name: None,
                content: vec![Block::text("tool result")],
                is_error: false,
                cache: None,
            }
        })
    }
}

#[tokio::test]
async fn agent_executes_host_tools_and_replays_encrypted_reasoning() {
    let mock = Arc::new(Mock::default());
    let reasoning = json!({"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"account-bound-opaque"});
    mock.replies.lock().unwrap().extend([
        sse_response(turn(vec![reasoning.clone(), json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"echo","arguments":"{\"value\":\"hi\"}"})]), true),
        sse_response(turn(vec![output_message()]), true),
    ]);
    let (core, endpoint, _) = setup(mock.clone(), Arc::new(MemoryStore::new(credentials(false))));
    let tools = Echo(AtomicUsize::new(0));
    let agent = Agent::new(&core, &endpoint, Arc::new(TokioClock)).tools(&tools);
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut run = agent.run(
            req(true),
            RunOptions {
                max_steps: 3,
                ..Default::default()
            },
        );
        let mut done = false;
        while let Some(event) = run.next().await {
            if let LoopEvent::Done(output) = event {
                assert!(
                    matches!(output.stop, RunStop::Finished(Some(StopReason::EndTurn))),
                    "{:?}",
                    output.stop
                );
                done = true;
            }
        }
        assert!(done);
    })
    .await
    .unwrap();
    assert_eq!(tools.0.load(Ordering::SeqCst), 1);
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let body: Value = serde_json::from_slice(&seen[1].body).unwrap();
    let input = body["input"].as_array().unwrap();
    assert!(input.iter().any(|v| v == &reasoning));
    assert!(
        input
            .iter()
            .any(|v| v["type"] == "function_call_output" && v["call_id"] == "call_1")
    );
}
