use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use ncore::core::ir::{Block, Message, Request, Role};
use ncore::rt::{BoxFuture, boxed, header};
use ncore::{
    CancelToken, Core, GenerateOptions, HttpRequest, HttpResponse, InboundRequest, PassthroughBody,
    PassthroughOptions, Transport, TransportError,
};
use ncore_claudesub::{ClaudeSub, Config, CredentialStore, Credentials, MemoryStore, request};
use serde_json::{Value, json};

fn credentials(expired: bool) -> Credentials {
    Credentials {
        access_token: "old-access-secret".into(),
        refresh_token: Some("old-refresh-secret".into()),
        expires_at: if expired { 0 } else { u64::MAX },
        subscription_type: Some("pro".into()),
        rate_limit_tier: None,
    }
}

fn response(status: u16, value: Value) -> HttpResponse {
    raw_response(status, value.to_string())
}

fn raw_response(status: u16, body: String) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![],
        body: Box::pin(futures_util::stream::iter(
            body.into_bytes()
                .chunks(3)
                .map(|b| Ok(Bytes::copy_from_slice(b)))
                .collect::<Vec<_>>(),
        )),
    }
}

fn message() -> Value {
    json!({"type":"message","id":"msg_1","model":"fixture","role":"assistant",
        "content":[{"type":"text","text":"OK"}],"stop_reason":"end_turn",
        "usage":{"input_tokens":4,"output_tokens":1,"cache_read_input_tokens":9,"cache_creation_input_tokens":2}})
}

#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<HttpRequest>>,
    refreshes: AtomicUsize,
    reject_old: bool,
    monitor: Mutex<VecDeque<HttpResponse>>,
}

impl Transport for Mock {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.seen.lock().unwrap().push(req.clone());
        boxed(async move {
            if req.url == ncore_claudesub::oauth::TOKEN_URL {
                self.refreshes.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
                let v: Value = serde_json::from_slice(&req.body).unwrap();
                assert_eq!(v["refresh_token"], "old-refresh-secret");
                return Ok(response(
                    200,
                    json!({"access_token":"new-access-secret","refresh_token":"rotated-refresh-secret","expires_in":3600}),
                ));
            }
            if req.url.contains("/api/oauth/") {
                return Ok(self
                    .monitor
                    .lock()
                    .unwrap()
                    .pop_front()
                    .expect("unexpected monitor request"));
            }
            if self.reject_old
                && header(&req.headers, "authorization") == Some("Bearer old-access-secret")
            {
                return Ok(response(
                    401,
                    json!({"error":{"type":"authentication_error"}}),
                ));
            }
            let v: Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(v["system"][0]["text"], request::IDENTITY);
            assert!(v["system"][0].get("cache_control").is_none());
            assert!(header(&req.headers, "x-api-key").is_none());
            let mut r = response(200, message());
            r.headers = vec![
                (
                    "Anthropic-Ratelimit-Unified-5h-Utilization".into(),
                    "0.13".into(),
                ),
                (
                    "anthropic-ratelimit-unified-7d-utilization".into(),
                    "0.27".into(),
                ),
                (
                    "anthropic-ratelimit-unified-5h-reset".into(),
                    "1793110200".into(),
                ),
                (
                    "anthropic-ratelimit-unified-status".into(),
                    "allowed".into(),
                ),
            ];
            Ok(r)
        })
    }
}

fn req() -> Request {
    Request {
        model: "fixture".into(),
        system: vec![Block::text("Host system")],
        messages: vec![Message::new(Role::User, vec![Block::text("OK")])],
        max_tokens: Some(32),
        ..Default::default()
    }
}

#[test]
fn prefix_preserves_raw_content_and_all_four_cache_marks() {
    let raw = br#"{ "model":"fixture", "system":[ {"type":"text", "text":"Host", "cache_control":{"type":"ephemeral","ttl":"1h"}, "future":1.00} ], "tools":[{"name":"nabu_echo","cache_control":{"type":"ephemeral","ttl":"1h"}}], "messages":[{"role":"assistant","content":[{"type":"thinking","thinking":"x","signature":"raw-signature"},{"type":"tool_use","id":"a","name":"nabu_echo","input":{ "z":1.00, "a":2 }}]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"x","cache_control":{"type":"ephemeral"}},{"type":"text","text":"OK","cache_control":{"type":"ephemeral"}}]}], "unknown":1.000 }"#;
    let out = request::prepare(raw).unwrap();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["system"][0]["text"], request::IDENTITY);
    assert_eq!(v["system"][1]["future"], 1.0);
    let text = String::from_utf8(out.to_vec()).unwrap();
    assert_eq!(text.matches("cache_control").count(), 4);
    assert!(text.contains(r#""input":{ "z":1.00, "a":2 }"#));
    assert!(text.contains(r#""unknown":1.000"#));
    assert!(text.contains(r#""future":1.00"#));
    assert_eq!(request::prepare(&out).unwrap(), out);
}

#[test]
fn missing_string_and_invalid_system_forms() {
    for raw in [
        br#"{"messages":[]}"#.as_slice(),
        br#"{"system":"Host","messages":[]}"#,
    ] {
        let v: Value = serde_json::from_slice(&request::prepare(raw).unwrap()).unwrap();
        assert_eq!(v["system"][0]["text"], request::IDENTITY);
    }
    for raw in [br#"{"system":null}"#.as_slice(), b"[]", b"not json"] {
        assert!(request::prepare(raw).is_err());
    }
    let c = credentials(false);
    let debug = format!("{c:?}");
    assert!(!debug.contains("old-access-secret"));
    assert!(!debug.contains("old-refresh-secret"));
    let imported = Credentials::from_claude_json(br#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":123,"scopes":["x"]},"mcpOAuth":{}}"#).unwrap();
    assert_eq!(imported.expires_at, 123);
}

#[tokio::test]
async fn generation_snapshot_matches_request_hooks_and_header_limits_are_normalized() {
    let transport = Arc::new(Mock::default());
    let plugin = Arc::new(ClaudeSub::new(Arc::new(MemoryStore::new(credentials(
        false,
    )))));
    let ep = ClaudeSub::endpoint().with_plugin(plugin.clone());
    let core = Core::builder(transport.clone()).build();
    let options = GenerateOptions {
        cache: Some(ncore::CachePolicy::default()),
        ..Default::default()
    };
    let mut generation = core
        .generate(&ep, &req(), &options, &CancelToken::new())
        .await
        .unwrap();
    assert_eq!(generation.request, transport.seen.lock().unwrap()[0].body);
    let v: Value = serde_json::from_slice(&generation.request).unwrap();
    assert_eq!(v["system"][1]["text"], "Host system");
    assert_eq!(v["system"][1]["cache_control"]["type"], "ephemeral");
    assert_eq!(
        generation.take_response().unwrap().usage.cache_read,
        Some(9)
    );
    let limits = plugin.last_limits().unwrap();
    assert_eq!(limits.five_hour.used_percent, Some(13.0));
    assert_eq!(limits.seven_day.used_percent, Some(27.0));
    assert_eq!(limits.status.as_deref(), Some("allowed"));
}

#[tokio::test]
async fn concurrent_expired_requests_refresh_once_and_persist_rotation() {
    let transport = Arc::new(Mock::default());
    let store = Arc::new(MemoryStore::new(credentials(true)));
    let plugin = Arc::new(ClaudeSub::new(store.clone()));
    let ep = ClaudeSub::endpoint().with_plugin(plugin);
    let core = Core::builder(transport.clone()).build();
    let request = req();
    let options = GenerateOptions::default();
    let cancel = CancelToken::new();
    let outcomes = futures_util::future::join_all(
        (0..8).map(|_| core.generate(&ep, &request, &options, &cancel)),
    )
    .await;
    for outcome in outcomes {
        assert_eq!(outcome.unwrap().status, 200);
    }
    assert_eq!(transport.refreshes.load(Ordering::SeqCst), 1);
    let current = store.load().await.unwrap();
    assert_eq!(
        current.refresh_token.as_deref(),
        Some("rotated-refresh-secret")
    );
    assert!(
        transport
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.url.contains("/v1/messages"))
            .all(|r| header(&r.headers, "authorization") == Some("Bearer new-access-secret"))
    );
}

#[tokio::test]
async fn server_401_refreshes_and_replays_once() {
    let transport = Arc::new(Mock {
        reject_old: true,
        ..Default::default()
    });
    let plugin = Arc::new(ClaudeSub::new(Arc::new(MemoryStore::new(credentials(
        false,
    )))));
    let ep = ClaudeSub::endpoint().with_plugin(plugin);
    let core = Core::builder(transport.clone()).build();
    core.generate(
        &ep,
        &req(),
        &GenerateOptions::default(),
        &CancelToken::new(),
    )
    .await
    .unwrap();
    let seen = transport.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].body, seen[2].body);
    assert_eq!(transport.refreshes.load(Ordering::SeqCst), 1);
}

struct InterruptibleStore {
    value: MemoryStore,
    saves: AtomicUsize,
    started: tokio::sync::Notify,
    fail: bool,
}

impl CredentialStore for InterruptibleStore {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>> {
        self.value.load()
    }
    fn save<'a>(&'a self, c: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            if self.saves.fetch_add(1, Ordering::SeqCst) == 0 {
                self.started.notify_one();
                if self.fail {
                    return Err("disk unavailable".into());
                }
                return futures_util::future::pending().await;
            }
            self.value.save(c).await
        })
    }
}

#[tokio::test]
async fn failed_or_cancelled_persistence_never_refreshes_the_old_token_again() {
    for fail in [true, false] {
        let transport = Arc::new(Mock::default());
        let store = Arc::new(InterruptibleStore {
            value: MemoryStore::new(credentials(true)),
            saves: AtomicUsize::new(0),
            started: tokio::sync::Notify::new(),
            fail,
        });
        let plugin = Arc::new(ClaudeSub::new(store.clone()));
        let ep = ClaudeSub::endpoint().with_plugin(plugin);
        let core = Core::builder(transport.clone()).build();
        let request = req();
        let options = GenerateOptions::default();
        let cancel = CancelToken::new();
        if fail {
            assert!(
                core.generate(&ep, &request, &options, &cancel)
                    .await
                    .is_err()
            );
        } else {
            let future = core.generate(&ep, &request, &options, &cancel);
            tokio::pin!(future);
            tokio::select! {
                _ = store.started.notified() => {},
                _ = &mut future => panic!("persistence should be pending"),
            }
            // future drops at the end of this block while the store save is pending.
        }
        core.generate(&ep, &request, &options, &cancel)
            .await
            .unwrap();
        assert_eq!(transport.refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(
            store.load().await.unwrap().refresh_token.as_deref(),
            Some("rotated-refresh-secret")
        );
    }
}

#[tokio::test]
async fn monitoring_is_coalesced_and_null_is_not_zero() {
    let transport = Mock::default();
    transport.monitor.lock().unwrap().push_back(response(
        200,
        json!({
            "five_hour":{"utilization":13,"resets_at":"2026-10-27T14:10:00Z"},
            "seven_day":{"utilization":null},"seven_day_opus":null,"limits":null,
            "future_field":{"retained":true}
        }),
    ));
    let plugin = ClaudeSub::new(Arc::new(MemoryStore::new(credentials(false))));
    let results = futures_util::future::join_all((0..6).map(|_| plugin.usage(&transport))).await;
    for result in results {
        let value = result.unwrap();
        assert!(!value.stale);
        assert_eq!(
            value.value.five_hour.unwrap().remaining_percent(),
            Some(87.0)
        );
        assert_eq!(value.value.seven_day.unwrap().remaining_percent(), None);
        assert_eq!(value.value.extra["future_field"]["retained"], true);
    }
    assert_eq!(transport.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn usage_429_returns_stale_snapshot_and_does_not_block_inference() {
    let transport = Arc::new(Mock::default());
    transport
        .monitor
        .lock()
        .unwrap()
        .push_back(response(200, json!({"five_hour":{"utilization":13}})));
    let mut limited = response(429, json!({"error":"rate limited"}));
    limited.headers.push(("Retry-After".into(), "120".into()));
    transport.monitor.lock().unwrap().push_back(limited);
    let plugin = Arc::new(ClaudeSub::with_config(
        Arc::new(MemoryStore::new(credentials(false))),
        Config {
            usage_ttl: Duration::ZERO,
            ..Default::default()
        },
    ));
    assert!(!plugin.usage(transport.as_ref()).await.unwrap().stale);
    assert!(plugin.usage(transport.as_ref()).await.unwrap().stale);
    assert!(plugin.usage(transport.as_ref()).await.unwrap().stale);
    assert_eq!(transport.seen.lock().unwrap().len(), 2);
    let ep = ClaudeSub::endpoint().with_plugin(plugin);
    Core::builder(transport)
        .build()
        .generate(
            &ep,
            &req(),
            &GenerateOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
}

#[test]
fn profile_keeps_plan_separate_from_subscription_status() {
    let p: ncore_claudesub::Profile = serde_json::from_value(json!({
        "account":{"has_claude_pro":true,"email":"do-not-retain@example.test"},
        "organization":{"organization_type":"claude_pro","subscription_status":"canceled","billing_type":"apple_subscription"}
    })).unwrap();
    assert_eq!(p.plan(), Some("pro"));
    assert_eq!(
        p.organization.subscription_status.as_deref(),
        Some("canceled")
    );
    assert!(!serde_json::to_string(&p).unwrap().contains("do-not-retain"));
}

struct StreamMock;
impl Transport for StreamMock {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        boxed(async move {
            let body: Value = serde_json::from_slice(&req.body).unwrap();
            assert_eq!(body["tools"][0]["name"], "nabu_echo");
            let events = [
                json!({"type":"message_start","message":{"id":"m","type":"message","role":"assistant","model":"fixture","content":[],"usage":{"input_tokens":1,"output_tokens":0}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t","name":"nabu_echo","input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"value\":\"Привет\"}"}}),
                json!({"type":"content_block_stop","index":0}),
                json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":8}}),
                json!({"type":"message_stop"}),
            ];
            let text = events
                .iter()
                .map(|v| format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()))
                .collect::<String>();
            Ok(raw_response(200, text))
        })
    }
}

#[tokio::test]
async fn passthrough_sse_preserves_tool_names_utf8_and_events() {
    let plugin = Arc::new(ClaudeSub::new(Arc::new(MemoryStore::new(credentials(
        false,
    )))));
    let core = Core::builder(Arc::new(StreamMock)).build();
    let ep = ClaudeSub::endpoint().with_plugin(plugin);
    let inbound = InboundRequest { method:"POST".into(), path_and_query:"/v1/messages".into(), headers:vec![],
        body:Bytes::from(json!({"model":"fixture","stream":true,"max_tokens":64,"tools":[{"name":"nabu_echo"}],"messages":[{"role":"user","content":"hi"}]}).to_string()) };
    let response = core
        .passthrough(
            &ep,
            inbound,
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let PassthroughBody::Stream(mut stream) = response.body else {
        panic!("expected SSE")
    };
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk.unwrap());
    }
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.contains("nabu_echo"));
    assert!(text.contains("Привет"));
    assert!(text.contains("message_stop"));
    assert_eq!(stream.observation().usage.output_total, Some(8));
}

#[tokio::test]
async fn rejects_other_origins_before_loading_or_sending_credentials() {
    let transport = Arc::new(Mock::default());
    let plugin = Arc::new(ClaudeSub::new(Arc::new(MemoryStore::new(credentials(
        false,
    )))));
    let core = Core::builder(transport.clone()).build();
    let ep = ncore::Endpoint::new(
        ncore::Format::AnthropicMessages,
        "https://api.anthropic.com.evil.test",
    )
    .with_plugin(plugin);
    assert!(
        core.generate(
            &ep,
            &req(),
            &GenerateOptions::default(),
            &CancelToken::new()
        )
        .await
        .is_err()
    );
    assert!(transport.seen.lock().unwrap().is_empty());
}

struct Script {
    replies: Mutex<VecDeque<HttpResponse>>,
    seen: Mutex<Vec<HttpRequest>>,
}

impl Script {
    fn new(replies: Vec<HttpResponse>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            seen: Mutex::default(),
        }
    }
}

impl Transport for Script {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.seen.lock().unwrap().push(req);
        boxed(async {
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request"))
        })
    }
}

#[tokio::test]
async fn refresh_without_rotation_preserves_refresh_token_and_repeated_401_stops() {
    let transport = Arc::new(Script::new(vec![
        response(200, json!({"access_token":"new","expires_in":3600})),
        response(200, message()),
    ]));
    let store = Arc::new(MemoryStore::new(credentials(true)));
    let plugin = Arc::new(ClaudeSub::new(store.clone()));
    Core::builder(transport)
        .build()
        .generate(
            &ClaudeSub::endpoint().with_plugin(plugin),
            &req(),
            &GenerateOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        store.load().await.unwrap().refresh_token.as_deref(),
        Some("old-refresh-secret")
    );

    let transport = Arc::new(Script::new(vec![
        response(401, json!({"error":{"type":"authentication_error"}})),
        response(200, json!({"access_token":"new","expires_in":3600})),
        response(401, json!({"error":{"type":"authentication_error"}})),
    ]));
    let plugin = Arc::new(ClaudeSub::new(Arc::new(MemoryStore::new(credentials(
        false,
    )))));
    let result = Core::builder(transport.clone())
        .build()
        .generate(
            &ClaudeSub::endpoint().with_plugin(plugin),
            &req(),
            &GenerateOptions::default(),
            &CancelToken::new(),
        )
        .await;
    assert!(matches!(result, Err(ncore::Error::Provider(_))));
    assert_eq!(transport.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn refresh_failure_has_cooldown_and_does_not_expose_response_secrets() {
    let transport = Arc::new(Script::new(vec![response(
        400,
        json!({"error":"invalid_grant","secret":"old-refresh-secret"}),
    )]));
    let plugin = Arc::new(ClaudeSub::new(Arc::new(MemoryStore::new(credentials(
        true,
    )))));
    let core = Core::builder(transport.clone()).build();
    let ep = ClaudeSub::endpoint().with_plugin(plugin);
    for _ in 0..2 {
        let error = core
            .generate(
                &ep,
                &req(),
                &GenerateOptions::default(),
                &CancelToken::new(),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("old-refresh-secret"));
    }
    assert_eq!(transport.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn oauth_pkce_and_state_are_validated_before_exchange() {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    let start = ncore_claudesub::oauth::start_login().unwrap();
    let transport = Script::new(vec![]);
    assert!(
        ncore_claudesub::oauth::finish_login(&transport, start, "code", "wrong-state")
            .await
            .is_err()
    );
    assert!(transport.seen.lock().unwrap().is_empty());

    let start = ncore_claudesub::oauth::start_login().unwrap();
    let query = start.url.split_once('?').unwrap().1;
    let state = query
        .split('&')
        .find_map(|p| p.strip_prefix("state="))
        .unwrap()
        .to_owned();
    let challenge = query
        .split('&')
        .find_map(|p| p.strip_prefix("code_challenge="))
        .unwrap()
        .to_owned();
    let transport = Script::new(vec![response(
        200,
        json!({"access_token":"access","refresh_token":"refresh","expires_in":3600}),
    )]);
    let c = ncore_claudesub::oauth::finish_login(&transport, start, "code", &state)
        .await
        .unwrap();
    assert_eq!(c.refresh_token.as_deref(), Some("refresh"));
    let seen = transport.seen.lock().unwrap();
    let body: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(body["state"], state);
    let actual = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(
        body["code_verifier"].as_str().unwrap().as_bytes(),
    ));
    assert_eq!(actual, challenge);
    assert_eq!(
        body["redirect_uri"],
        ncore_claudesub::oauth::REDIRECT_URI
    );
}
