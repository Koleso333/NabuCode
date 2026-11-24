use bytes::Bytes;
use futures_util::{StreamExt, stream};
use ncore::rt::{BoxFuture, boxed, header};
use ncore::{
    CancelToken, Core, Hook, HttpRequest, HttpResponse, InboundRequest, PassthroughBody,
    PassthroughOptions, RequestCtx, Transport, TransportError,
};
use ncore_workbuddyacc::{
    Access, Catalog, ClientInfo, CredentialStore, Credentials, Credits, MemoryStore, WorkBuddyAcc,
    catalog::promotion_active, oauth,
};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<HttpRequest>>,
    replies: Mutex<VecDeque<HttpResponse>>,
}
impl Transport for Mock {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.seen.lock().unwrap().push(req);
        boxed(async {
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected network call"))
        })
    }
}
fn c() -> Credentials {
    Credentials {
        access_token: "private-access".into(),
        refresh_token: Some("private-refresh".into()),
        expires_at: u64::MAX,
        refresh_expires_at: None,
        uid: "user-1".into(),
        domain: "www.workbuddy.ai".into(),
    }
}
fn response(status: u16, v: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: Box::pin(stream::iter([Ok(Bytes::from(v.to_string()))])),
    }
}
fn sse(complete: bool) -> HttpResponse {
    let mut text = format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        json!({"id":"chat-1","model":"hy4free","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"think"}}]}),
        json!({"choices":[{"index":0,"delta":{"content":"OK"}}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":4,"total_tokens":13}})
    );
    if complete {
        text.push_str("data: [DONE]\n\n");
    }
    HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: Box::pin(stream::iter(
            text.as_bytes()
                .chunks(3)
                .map(|s| Ok(Bytes::copy_from_slice(s)))
                .collect::<Vec<_>>(),
        )),
    }
}
fn setup(
    mock: Arc<Mock>,
    credentials: Credentials,
) -> (
    Core,
    ncore::Endpoint,
    Arc<WorkBuddyAcc>,
    Arc<MemoryStore>,
) {
    let store = Arc::new(MemoryStore::new(credentials));
    let plugin = Arc::new(WorkBuddyAcc::new(store.clone(), ClientInfo::new("5.9.4")).unwrap());
    (
        Core::builder(mock).build(),
        WorkBuddyAcc::endpoint().with_plugin(plugin.clone()),
        plugin,
        store,
    )
}
fn incoming(body: Value) -> InboundRequest {
    InboundRequest {
        method: "POST".into(),
        path_and_query: "/v1/chat/completions".into(),
        headers: vec![
            ("authorization".into(), "Bearer caller-secret".into()),
            ("cookie".into(), "caller-cookie".into()),
        ],
        body: Bytes::from(body.to_string()),
    }
}
fn body(stream: bool) -> Value {
    json!({"model":"hy4free","stream":stream,"messages":[{"role":"system","content":"My caller instructions, OpenAI and Codex stay verbatim."},{"role":"assistant","reasoning_content":"signed-history","tool_calls":[{"id":"t","type":"function","function":{"name":"echo","arguments":"{}"}}]},{"role":"tool","tool_call_id":"t","content":"result"},{"role":"user","content":"Hi"}],"tools":[{"type":"function","function":{"name":"echo","parameters":{"type":"object","properties":{}}}}],"temperature":0.1})
}

#[tokio::test]
async fn native_history_tools_and_headers_are_preserved() {
    let mock = Arc::new(Mock::default());
    mock.replies.lock().unwrap().push_back(sse(true));
    let (core, ep, _, _) = setup(mock.clone(), c());
    let original = body(true);
    let result = core
        .passthrough(
            &ep,
            incoming(original.clone()),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let PassthroughBody::Stream(mut stream) = result.body else {
        panic!("expected stream")
    };
    while let Some(c) = stream.next().await {
        c.unwrap();
    }
    let seen = mock.seen.lock().unwrap();
    let req = &seen[0];
    assert_eq!(req.url, "https://www.workbuddy.ai/v2/chat/completions");
    assert_eq!(
        serde_json::from_slice::<Value>(&req.body).unwrap(),
        original
    );
    assert_eq!(
        header(&req.headers, "authorization"),
        Some("Bearer private-access")
    );
    assert!(header(&req.headers, "cookie").is_none());
    assert_eq!(header(&req.headers, "x-agent-intent"), Some("craft"));
    assert_eq!(header(&req.headers, "x-conversation-id").unwrap().len(), 32);
}

#[tokio::test]
async fn nonstream_collects_fragmented_sse_reasoning_text_and_usage() {
    let mock = Arc::new(Mock::default());
    mock.replies.lock().unwrap().push_back(sse(true));
    let (core, ep, _, _) = setup(mock.clone(), c());
    let result = core
        .passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let PassthroughBody::Full { bytes, .. } = result.body else {
        panic!("expected JSON")
    };
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "OK");
    assert_eq!(v["choices"][0]["message"]["reasoning_content"], "think");
    assert_eq!(v["usage"]["prompt_tokens"], 9);
    assert_eq!(
        serde_json::from_slice::<Value>(&mock.seen.lock().unwrap()[0].body).unwrap()["stream"],
        true
    );
}

#[tokio::test]
async fn expired_session_refreshes_and_saves_rotated_tokens() {
    let mock = Arc::new(Mock::default());
    mock.replies.lock().unwrap().extend([response(200,json!({"code":0,"data":{"accessToken":"fresh-access","refreshToken":"fresh-refresh","expiresIn":3600,"refreshExpiresIn":7200,"domain":"www.workbuddy.ai"}})),response(200,json!({"code":0,"data":{"Packages":[],"TotalCredits":"0","IsPaidUser":false}}))]);
    let mut old = c();
    old.expires_at = 0;
    let (_, _, plugin, store) = setup(mock.clone(), old);
    assert_eq!(plugin.credits(&*mock).await.unwrap().remaining(), Some("0"));
    let saved = store.load().await.unwrap();
    assert_eq!(saved.access_token, "fresh-access");
    assert_eq!(saved.refresh_token.as_deref(), Some("fresh-refresh"));
    let seen = mock.seen.lock().unwrap();
    assert!(seen[0].url.ends_with("/v2/plugin/auth/token/refresh"));
    assert_eq!(
        header(&seen[0].headers, "x-refresh-token"),
        Some("private-refresh")
    );
    assert_eq!(
        header(&seen[1].headers, "authorization"),
        Some("Bearer fresh-access")
    );
}

#[tokio::test]
async fn auth_rejection_refreshes_once_and_business_error_retains_code() {
    let mock = Arc::new(Mock::default());
    mock.replies.lock().unwrap().extend([
        response(401, json!({"code":401})),
        response(
            200,
            json!({"code":0,"data":{"accessToken":"fresh","expiresIn":3600}}),
        ),
        response(
            400,
            json!({"code":11158,"msg":"model not allowed","displayMsg":{"en":"Plan required"}}),
        ),
    ]);
    let (core, ep, _, _) = setup(mock.clone(), c());
    let result = core
        .passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, 400);
    let PassthroughBody::Full { bytes, .. } = result.body else {
        panic!()
    };
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["error"]["code"], "11158");
    assert_eq!(v["error"]["message"], "Plan required");
    assert_eq!(mock.seen.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn login_handles_pending_and_consumes_ready_state() {
    let mock = Mock::default();
    mock.replies.lock().unwrap().extend([response(200,json!({"code":0,"data":{"state":"private-state","authUrl":"https://www.workbuddy.ai/login?state=private-state"}})),response(200,json!({"code":11217})),response(200,json!({"code":0,"data":{"uid":"u","accessToken":"a","refreshToken":"r","expiresIn":3600,"domain":"www.workbuddy.ai"}}))]);
    let client = ClientInfo::new("5.9.4");
    let mut login = oauth::start_login(&mock, &client).await.unwrap();
    assert!(!format!("{login:?}").contains("private-state"));
    assert!(matches!(
        oauth::poll_login(&mock, &client, &mut login).await.unwrap(),
        oauth::LoginPoll::Pending
    ));
    let oauth::LoginPoll::Ready(c) = oauth::poll_login(&mock, &client, &mut login).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(c.access_token, "a");
    assert!(oauth::poll_login(&mock, &client, &mut login).await.is_err());
}

#[tokio::test]
async fn refuses_other_origins_and_wrong_account_region_without_network() {
    let mock = Arc::new(Mock::default());
    let (_, _, plugin, _) = setup(mock.clone(), c());
    let mut req = HttpRequest {
        method: "POST".into(),
        url: "https://evil.invalid/v2/chat/completions".into(),
        headers: vec![],
        body: Bytes::from(body(true).to_string()),
    };
    assert!(
        plugin
            .on_request(
                &RequestCtx {
                    id: 1,
                    format: ncore::Format::OpenAiChat,
                    streaming: true,
                    attempt: 1
                },
                &mut req
            )
            .await
            .is_err()
    );
    let mut wrong = c();
    wrong.domain = "copilot.tencent.com".into();
    let (_, _, plugin, _) = setup(mock.clone(), wrong);
    assert!(plugin.credits(&*mock).await.is_err());
    assert!(mock.seen.lock().unwrap().is_empty());
    assert!(!format!("{:?}", c()).contains("private-access"));
}

#[test]
fn catalog_separates_plan_free_disabled_and_paid_pair() {
    let config = json!({"models":[{"id":"hy4free","name":"Hy4","credits":"x0.00"},{"id":"hy4std","name":"Hy4","credits":"x0.29"},{"id":"pro-model","name":"Pro"},{"id":"off","disabled":true},{"id":"image","tags":["text-to-image"]}],"modelTiers":[{"modelIds":["pro-model"],"requiredUserType":"standard","enabled":true}],"productFeaturesConfig":{"ModelRateLimitCap":{"lines":[{"freeId":"hy4free","paidId":"hy4std"}]}}});
    let credits = Credits {
        raw: json!({"IsPaidUser":false,"TotalCredits":"0","SubscriptionPackageName":"Free"}),
        fetched_at: 0,
    };
    let cat = Catalog::from_config(config.clone(), Some(&credits), 0).unwrap();
    assert_eq!(cat.models.len(), 4);
    assert_eq!(cat.models[0].display_name, "Hy4 Free");
    assert_eq!(cat.models[0].paid_alternative.as_deref(), Some("hy4std"));
    assert_eq!(cat.models[1].display_name, "Hy4");
    assert_eq!(cat.models[2].access, Access::PlanRequired);
    assert_eq!(cat.models[3].access, Access::Disabled);
    assert_eq!(cat.available().count(), 2);
    assert_eq!(
        Catalog::from_config(config, None, 0).unwrap().models[2].access,
        Access::Unknown
    );
}

#[test]
fn promotions_obey_end_exclusive_and_overnight_timezone() {
    let now = chrono::DateTime::parse_from_rfc3339("2026-11-10T23:30:00+08:00")
        .unwrap()
        .timestamp_millis() as u64;
    let mut p = json!({"enabled":true,"schedule":{"validFrom":"2026-11-10T00:00:00+08:00","validUntil":"2026-11-11T00:00:00+08:00","timezone":"Asia/Shanghai","daily":[{"start":"23:00","end":"01:00"}]}});
    assert!(promotion_active(&p, now));
    assert!(!promotion_active(&p, now + 30 * 60 * 1000));
    p["enabled"] = false.into();
    assert!(!promotion_active(&p, now));
    let p =
        json!({"schedule":{"timezone":"Asia/Shanghai","daily":[{"start":"01:00","end":"02:00"}]}});
    assert!(!promotion_active(&p, now));
}

#[test]
fn active_free_promotion_overrides_price_until_expiry() {
    let config = json!({"models":[{"id":"m","name":"M","credits":"x0.29"}],"modelPromotions":[{"id":"sale","enabled":true,"modelIds":["m"],"discount":{"factor":0,"discountedCredits":"0x"},"schedule":{"validFrom":"2026-11-10T00:00:00Z","validUntil":"2026-11-11T00:00:00Z"}}]});
    let now = chrono::DateTime::parse_from_rfc3339("2026-11-10T00:00:00Z")
        .unwrap()
        .timestamp_millis() as u64;
    let cat = Catalog::from_config(config.clone(), None, now).unwrap();
    assert_eq!(cat.models[0].display_name, "M Free");
    assert!(cat.models[0].free);
    let cat = Catalog::from_config(config, None, now + 86400000).unwrap();
    assert!(!cat.models[0].free);
    assert_eq!(cat.models[0].credits.as_deref(), Some("x0.29"));
}

#[tokio::test]
async fn empty_native_finish_reason_does_not_close_tool_argument_stream() {
    use ncore::core::{
        SseParser, codec,
        ir::{Accumulator, Block},
    };
    for streaming in [true, false] {
        let mock = Arc::new(Mock::default());
        let chunks = [
            json!({"id":"c","model":"m","choices":[{"delta":{"reasoning_content":"think"}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"","tool_calls":[{"index":0,"id":"t","type":"function","function":{"name":"echo","arguments":""}}]}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"","tool_calls":[{"index":0,"function":{"arguments":"{\"x\":"}}]}}]}),
            json!({"choices":[{"delta":{"reasoning_content":"","tool_calls":[{"index":0,"function":{"arguments":"1}"}}]}}]}),
            json!({"choices":[{"delta":{"reasoning_content":""},"finish_reason":"tool_calls"}]}),
        ];
        let mut raw = chunks
            .iter()
            .cloned()
            .map(|mut v| {
                if v["choices"][0].get("finish_reason").is_none() {
                    v["choices"][0]["finish_reason"] = "".into();
                }
                format!("data: {v}\n\n")
            })
            .collect::<String>();
        raw.push_str("data: [DONE]\n\n");
        mock.replies.lock().unwrap().push_back(HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: Box::pin(stream::iter(
                raw.as_bytes()
                    .chunks(5)
                    .map(|c| Ok(Bytes::copy_from_slice(c)))
                    .collect::<Vec<_>>(),
            )),
        });
        let (core, ep, _, _) = setup(mock.clone(), c());
        let mut input = body(streaming);
        input["tool_choice"] = json!({"type":"function","function":{"name":"echo"}});
        let output = core
            .passthrough(
                &ep,
                incoming(input),
                PassthroughOptions::default(),
                &CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&mock.seen.lock().unwrap()[0].body).unwrap()["tool_choice"],
            "echo"
        );
        let response = match output.body {
            PassthroughBody::Full { bytes, .. } => {
                codec::decode_response(ncore::Format::OpenAiChat, &bytes).unwrap()
            }
            PassthroughBody::Stream(mut s) => {
                let mut parser = SseParser::new();
                let mut decoder = codec::stream_decoder(ncore::Format::OpenAiChat);
                let mut acc = Accumulator::new();
                while let Some(chunk) = s.next().await {
                    for event in parser.feed(&chunk.unwrap()) {
                        for ev in decoder.feed(&event).unwrap() {
                            acc.push(&ev);
                        }
                    }
                }
                assert!(acc.is_done());
                acc.finish()
            }
        };
        let call = response
            .content
            .iter()
            .find_map(|b| {
                if let Block::ToolCall(c) = b {
                    Some(c)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(call.args, "{\"x\":1}");
        assert_eq!(
            response
                .content
                .iter()
                .filter(|b| matches!(b, Block::Thinking(_)))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn nonstream_rejects_truncated_upstream_without_done() {
    let mock = Arc::new(Mock::default());
    mock.replies.lock().unwrap().push_back(sse(false));
    let (core, ep, _, _) = setup(mock, c());
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
async fn concurrent_account_calls_share_a_single_refresh() {
    let mock = Arc::new(Mock::default());
    let summary = json!({"code":0,"data":{"Packages":[],"TotalCredits":"0","IsPaidUser":false}});
    mock.replies.lock().unwrap().extend([
        response(200,json!({"code":0,"data":{"accessToken":"fresh","refreshToken":"rotated","expiresIn":3600}})),
        response(200,summary.clone()),response(200,summary),
    ]);
    let mut old = c();
    old.expires_at = 0;
    let (_, _, plugin, _) = setup(mock.clone(), old);
    let (a, b) = tokio::join!(plugin.credits(&*mock), plugin.credits(&*mock));
    assert_eq!(a.unwrap().remaining(), Some("0"));
    assert_eq!(b.unwrap().remaining(), Some("0"));
    assert_eq!(
        mock.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.url.ends_with("/token/refresh"))
            .count(),
        1
    );
}

struct FailFirstSave {
    current: Mutex<Credentials>,
    fail: std::sync::atomic::AtomicBool,
}
impl CredentialStore for FailFirstSave {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>> {
        boxed(async { Ok(self.current.lock().unwrap().clone()) })
    }
    fn save<'a>(&'a self, c: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            if self.fail.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return Err("disk unavailable".into());
            }
            *self.current.lock().unwrap() = c.clone();
            Ok(())
        })
    }
}

#[tokio::test]
async fn rotated_token_is_retained_when_first_save_fails() {
    let mock = Mock::default();
    mock.replies.lock().unwrap().extend([
        response(200,json!({"code":0,"data":{"accessToken":"fresh","refreshToken":"rotated","expiresIn":3600}})),
        response(200,json!({"code":0,"data":{"Packages":[],"TotalCredits":"1.5"}})),
    ]);
    let mut old = c();
    old.expires_at = 0;
    let store = Arc::new(FailFirstSave {
        current: Mutex::new(old),
        fail: std::sync::atomic::AtomicBool::new(true),
    });
    let plugin = WorkBuddyAcc::new(store.clone(), ClientInfo::new("5.9.4")).unwrap();
    assert!(plugin.credits(&mock).await.is_err());
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
    assert_eq!(
        plugin.credits(&mock).await.unwrap().remaining(),
        Some("1.5")
    );
    assert_eq!(
        store.load().await.unwrap().refresh_token.as_deref(),
        Some("rotated")
    );
    assert_eq!(mock.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn failed_refresh_has_cooldown_and_metadata_failure_is_not_zero_balance() {
    let mock = Arc::new(Mock::default());
    mock.replies
        .lock()
        .unwrap()
        .push_back(response(500, json!({"code":500})));
    let mut old = c();
    old.expires_at = 0;
    let (_, _, plugin, _) = setup(mock.clone(), old);
    assert!(plugin.credits(&*mock).await.is_err());
    assert!(plugin.credits(&*mock).await.is_err());
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
    let mock = Arc::new(Mock::default());
    mock.replies
        .lock()
        .unwrap()
        .push_back(response(200, json!({"code":0,"data":{}})));
    let (_, _, plugin, _) = setup(mock.clone(), c());
    assert!(plugin.credits(&*mock).await.is_err());
}

#[tokio::test]
async fn browser_login_looks_up_account_uid_with_native_plugin_path() {
    let mock = Mock::default();
    mock.replies.lock().unwrap().extend([
        response(200,json!({"code":0,"data":{"state":"s","authUrl":"https://www.workbuddy.ai/login"}})),
        response(200,json!({"code":0,"data":{"accessToken":"a","expiresIn":3600,"domain":"www.workbuddy.ai"}})),
        response(200,json!({"code":0,"data":{"accounts":[{"uid":"u","lastLogin":true}]}})),
    ]);
    let client = ClientInfo::new("5.9.4");
    let mut login = oauth::start_login(&mock, &client).await.unwrap();
    let oauth::LoginPoll::Ready(c) = oauth::poll_login(&mock, &client, &mut login).await.unwrap()
    else {
        panic!()
    };
    assert_eq!(c.uid, "u");
    let seen = mock.seen.lock().unwrap();
    assert!(seen[2].url.ends_with("/v2/plugin/accounts"));
    assert_eq!(header(&seen[2].headers, "authorization"), Some("Bearer a"));
}

fn reasoning_config() -> Value {
    json!({"code":0,"data":{"models":[{"id":"hy4free","supportsReasoning":true,"onlyReasoning":true,"reasoning":{"supportedEfforts":["high"],"defaultEffort":"high","canDisableThinking":false}},{"id":"flexible","supportsReasoning":true,"onlyReasoning":false,"reasoning":{"supportedEfforts":["low","high","max"],"defaultEffort":"high","canDisableThinking":true}}]}})
}

#[tokio::test]
async fn explicit_reasoning_uses_native_fields_and_cached_capabilities() {
    let mock = Arc::new(Mock::default());
    mock.replies
        .lock()
        .unwrap()
        .extend([response(200, reasoning_config()), sse(true), sse(true)]);
    let (core, ep, _, _) = setup(mock.clone(), c());
    for effort in [
        json!({"effort":"high","summary":"auto"}),
        json!({"effort":"high"}),
    ] {
        let mut input = body(false);
        input["reasoning"] = effort;
        core.passthrough(
            &ep,
            incoming(input),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    }
    let seen = mock.seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    assert!(seen[0].url.ends_with("/v3/config"));
    for req in &seen[1..] {
        let v: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(v["reasoning_effort"], "high");
        assert_eq!(v["reasoning_summary"], "auto");
        assert!(v.get("reasoning").is_none());
    }
}

#[tokio::test]
async fn unsupported_effort_and_forbidden_off_never_reach_inference() {
    let mock = Arc::new(Mock::default());
    mock.replies
        .lock()
        .unwrap()
        .push_back(response(200, reasoning_config()));
    let (core, ep, _, _) = setup(mock.clone(), c());
    for effort in ["low", "none", "nabu_invalid"] {
        let mut input = body(false);
        input["reasoning_effort"] = effort.into();
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
    }
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn declared_optional_reasoning_off_uses_native_omission() {
    let mock = Arc::new(Mock::default());
    mock.replies
        .lock()
        .unwrap()
        .extend([response(200, reasoning_config()), sse(true)]);
    let (core, ep, _, _) = setup(mock.clone(), c());
    let mut input = body(false);
    input["model"] = "flexible".into();
    input["reasoning_effort"] = "none".into();
    input["reasoning_summary"] = "auto".into();
    core.passthrough(
        &ep,
        incoming(input),
        PassthroughOptions::default(),
        &CancelToken::new(),
    )
    .await
    .unwrap();
    let seen = mock.seen.lock().unwrap();
    let sent: Value = serde_json::from_slice(&seen[1].body).unwrap();
    assert!(sent.get("reasoning_effort").is_none());
    assert!(sent.get("reasoning_summary").is_none());
    assert!(sent.get("thinking").is_none());
}

#[tokio::test]
async fn native_cache_aliases_and_raw_usage_survive_stream_and_nonstream() {
    for streaming in [true, false] {
        let mock = Arc::new(Mock::default());
        let usage = json!({"prompt_tokens":100,"completion_tokens":9,"prompt_tokens_details":{"cached_tokens":0},"prompt_cache_hit_tokens":80,"cache_read_input_tokens":0,"cache_creation_input_tokens":0,"prompt_cache_write_tokens":7,"prompt_cache_miss_tokens":20,"completion_thinking_tokens":5,"credit":0.02,"quota":{"model":"m","percent":3}});
        let raw = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"id":"c","model":"hy4free","choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}),
            json!({"choices":[],"usage":usage})
        );
        mock.replies.lock().unwrap().push_back(HttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: Box::pin(stream::iter([Ok(Bytes::from(raw))])),
        });
        let (core, ep, plugin, _) = setup(mock, c());
        let result = core
            .passthrough(
                &ep,
                incoming(body(streaming)),
                PassthroughOptions::default(),
                &CancelToken::new(),
            )
            .await
            .unwrap();
        let actual = match result.body {
            PassthroughBody::Full { bytes, .. } => {
                serde_json::from_slice::<Value>(&bytes).unwrap()["usage"].clone()
            }
            PassthroughBody::Stream(mut stream) => {
                while let Some(chunk) = stream.next().await {
                    chunk.unwrap();
                }
                plugin.last_usage().unwrap().raw
            }
        };
        assert_eq!(actual["prompt_tokens"], 100);
        assert_eq!(actual["prompt_tokens_details"]["cached_tokens"], 80);
        assert_eq!(actual["prompt_tokens_details"]["cache_write_tokens"], 7);
        assert_eq!(actual["completion_tokens_details"]["reasoning_tokens"], 5);
        assert_eq!(actual["credit"], 0.02);
        assert_eq!(actual["quota"]["percent"], 3);
        assert_eq!(actual["prompt_cache_miss_tokens"], 20);
        assert_eq!(
            plugin.last_usage().unwrap().raw["prompt_cache_hit_tokens"],
            80
        );
    }
}

#[test]
fn capabilities_do_not_advertise_off_for_only_reasoning_models() {
    let config = reasoning_config()["data"].clone();
    let catalog = Catalog::from_config(config, None, 0).unwrap();
    assert!(!catalog.models[0].reasoning.can_disable);
    assert_eq!(catalog.models[0].reasoning.efforts, ["high"]);
    assert!(catalog.models[1].reasoning.can_disable);
}

#[tokio::test]
async fn normalized_cache_usage_survives_each_translated_format() {
    use ncore::core::{SseParser, codec, ir::Accumulator};
    use ncore::{Format, TranslateBody, TranslateOptions};
    for format in [
        Format::AnthropicMessages,
        Format::OpenAiResponses,
        Format::Gemini,
    ] {
        for streaming in [false, true] {
            let mock = Arc::new(Mock::default());
            let raw = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"id":"c","model":"hy4free","choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]}),
                json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":9,"prompt_cache_hit_tokens":80,"prompt_cache_write_tokens":7,"completion_thinking_tokens":5,"credit":0.02}})
            );
            mock.replies.lock().unwrap().push_back(HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: Box::pin(stream::iter([Ok(Bytes::from(raw))])),
            });
            let (core, ep, plugin, _) = setup(mock, c());
            let (path, body) = match format {
                Format::AnthropicMessages => (
                    "/v1/messages".to_owned(),
                    json!({"model":"hy4free","stream":streaming,"max_tokens":1024,"messages":[{"role":"user","content":"Hello"}]}),
                ),
                Format::OpenAiResponses => (
                    "/v1/responses".into(),
                    json!({"model":"hy4free","stream":streaming,"input":"Hello"}),
                ),
                Format::Gemini => (
                    format!(
                        "/v1beta/models/hy4free:{}",
                        if streaming {
                            "streamGenerateContent?alt=sse"
                        } else {
                            "generateContent"
                        }
                    ),
                    json!({"contents":[{"role":"user","parts":[{"text":"Hello"}]}]}),
                ),
                _ => unreachable!(),
            };
            let result = core
                .translate(
                    &ep,
                    InboundRequest {
                        method: "POST".into(),
                        path_and_query: path,
                        headers: vec![],
                        body: Bytes::from(body.to_string()),
                    },
                    TranslateOptions {
                        encode: WorkBuddyAcc::encode_options(),
                        ..Default::default()
                    },
                    &CancelToken::new(),
                )
                .await
                .unwrap();
            let usage = match result.body {
                TranslateBody::Full { bytes, .. } => {
                    codec::decode_response(format, &bytes).unwrap().usage
                }
                TranslateBody::Stream(mut stream) => {
                    let mut p = SseParser::new();
                    let mut decoder = codec::stream_decoder(format);
                    let mut a = Accumulator::new();
                    while let Some(chunk) = stream.next().await {
                        for e in p.feed(&chunk.unwrap()) {
                            for ev in decoder.feed(&e).unwrap() {
                                a.push(&ev);
                            }
                        }
                    }
                    for e in decoder.finish() {
                        a.push(&e);
                    }
                    a.finish().usage
                }
            };
            assert_eq!(
                usage.input_total,
                Some(100),
                "cached input counted twice for {format:?}"
            );
            assert_eq!(usage.cache_read, Some(80));
            assert_eq!(usage.reasoning, Some(5));
            if format == Format::AnthropicMessages {
                assert_eq!(usage.cache_write, Some(7));
            } else {
                assert_eq!(usage.cache_write, None);
            }
            assert_eq!(
                plugin.last_usage().unwrap().raw["prompt_tokens_details"]["cache_write_tokens"],
                7
            );
            assert_eq!(plugin.last_usage().unwrap().raw["credit"], 0.02);
        }
    }
}

#[tokio::test]
async fn cache_miss_is_not_claimed_as_an_explicit_cache_write() {
    let (_, _, plugin, _) = setup(Arc::new(Mock::default()), c());
    let ctx = RequestCtx {
        id: 1,
        format: ncore::Format::OpenAiChat,
        streaming: false,
        attempt: 1,
    };
    let mut bytes = Bytes::from(
        json!({"usage":{"prompt_tokens":100,"prompt_cache_miss_tokens":100}}).to_string(),
    );
    plugin.on_body(&ctx, 200, &mut bytes).unwrap();
    let raw = plugin.last_usage().unwrap().raw;
    assert!(
        raw.pointer("/prompt_tokens_details/cache_write_tokens")
            .is_none()
    );
    assert_eq!(raw["prompt_cache_miss_tokens"], 100);
}

#[tokio::test]
async fn collected_response_without_usage_does_not_invent_zero_counters() {
    let mock = Arc::new(Mock::default());
    let raw = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"model":"hy4free","choices":[{"delta":{"content":"OK"},"finish_reason":"stop"}]})
    );
    mock.replies.lock().unwrap().push_back(HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: Box::pin(stream::iter([Ok(Bytes::from(raw))])),
    });
    let (core, ep, plugin, _) = setup(mock, c());
    let output = core
        .passthrough(
            &ep,
            incoming(body(false)),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    let PassthroughBody::Full { bytes, .. } = output.body else {
        panic!()
    };
    assert!(
        serde_json::from_slice::<Value>(&bytes)
            .unwrap()
            .get("usage")
            .is_none()
    );
    assert!(plugin.last_usage().is_none());
}
