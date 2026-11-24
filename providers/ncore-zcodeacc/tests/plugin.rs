use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use futures_util::{StreamExt, stream};
use ncore::core::codec::EncodeOptions;
use ncore::core::ir::Block;
use ncore::rt::{BoxFuture, boxed, header};
use ncore::{
    CancelToken, Core, Error, Format, Hook, HttpRequest, HttpResponse, InboundRequest,
    PassthroughBody, PassthroughOptions, RequestCtx, TranslateBody, TranslateOptions, Transport,
    TransportError,
};
use ncore_zcodeacc::{
    ClientInfo, CredentialStore, Credentials, MemoryStore, UsageSnapshot, ZCodeAcc, oauth, request,
};
use serde_json::{Value, json};

fn credentials(account: &str) -> Credentials {
    Credentials::new(format!(
        "e30.{}.signature",
        URL_SAFE_NO_PAD.encode(json!({"sub":account}).to_string())
    ))
    .unwrap()
}

fn client() -> ClientInfo {
    ClientInfo::new("3.17.2", "test-device")
}

fn response(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: Box::pin(stream::iter([Ok(Bytes::from(value.to_string()))])),
    }
}

fn reply() -> Value {
    json!({"id":"msg_1","type":"message","role":"assistant","model":"glm-5.3-flash",
        "content":[{"type":"text","text":"NABU_OK"}],"stop_reason":"end_turn",
        "usage":{"input_tokens":300,"output_tokens":6}})
}

fn stream_reply() -> HttpResponse {
    let text = concat!(
        "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"glm-5.3-flash\",\"content\":[],\"usage\":{\"input_tokens\":300,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"NABU_OK\"}}\n\n",
        "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":6}}\n\n",
        "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
    );
    let chunks = text
        .as_bytes()
        .chunks(3)
        .map(|s| Ok(Bytes::copy_from_slice(s)))
        .collect::<Vec<_>>();
    HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: Box::pin(stream::iter(chunks)),
    }
}

#[derive(Default)]
struct Mock {
    seen: Mutex<Vec<HttpRequest>>,
    replies: Mutex<VecDeque<HttpResponse>>,
}

impl Transport for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.seen.lock().unwrap().push(request);
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

fn setup(mock: Arc<Mock>) -> (Core, ncore::Endpoint, Arc<ZCodeAcc>) {
    let plugin = Arc::new(ZCodeAcc::new(
        Arc::new(MemoryStore::new(credentials("one"))),
        client(),
    ));
    (
        Core::builder(mock).build(),
        ZCodeAcc::endpoint().with_plugin(plugin.clone()),
        plugin,
    )
}

fn incoming(body: Value) -> InboundRequest {
    InboundRequest {
        method: "POST".into(),
        path_and_query: "/v1/messages".into(),
        headers: vec![
            ("authorization".into(), "Bearer inbound-secret".into()),
            ("cookie".into(), "private-cookie".into()),
        ],
        body: Bytes::from(body.to_string()),
    }
}

#[test]
fn prepare_preserves_signed_history_schemas_cache_and_json_bytes() {
    let body = br#"{"model":"glm-5.3", "max_tokens":2048,"system":[{"type":"text","text":"Host A","cache_control":{"type":"ephemeral","ttl":"1h"}},{"type":"text","text":"Host B"}], "messages": [ {"role":"assistant","content":[{"type":"thinking","thinking":"old","signature":"sig"},{"type":"tool_use","id":"t1","name":"echo","input":{"z":1,"a":2}}]} ],"tools":[{"name":"echo","input_schema":{"type":"object","properties":{"z":{},"a":{}}}}]}"#;
    let prepared = request::prepare(body).unwrap();
    let value: Value = serde_json::from_slice(&prepared).unwrap();
    assert_eq!(
        value["system"][0]["text"],
        "You are ZCode, an interactive agent"
    );
    assert!(
        value["system"][1]["text"]
            .as_str()
            .unwrap()
            .ends_with(request::OVERRIDE)
    );
    assert_eq!(value["system"][2]["text"], "Host A\n\nHost B");
    assert_eq!(value["system"][2]["cache_control"]["ttl"], "1h");
    assert_eq!(value["system"].as_array().unwrap().len(), 3);
    for pointer in ["/messages", "/tools", "/max_tokens"] {
        let path = ncore::core::json::JsonPath::parse(pointer).unwrap();
        let old = ncore::core::json::find(body, &path).unwrap().unwrap();
        let new = ncore::core::json::find(&prepared, &path)
            .unwrap()
            .unwrap();
        assert_eq!(&body[old.start..old.end], &prepared[new.start..new.end]);
    }
    assert_eq!(request::prepare(&prepared).unwrap(), prepared);
}

#[test]
fn accepts_string_and_absent_system_but_rejects_invalid_shapes() {
    for source in [
        json!({"messages":[]}),
        json!({"system":"Host", "messages":[]}),
    ] {
        let prepared = request::prepare(source.to_string().as_bytes()).unwrap();
        let value: Value = serde_json::from_slice(&prepared).unwrap();
        assert_eq!(
            value["system"].as_array().unwrap().len(),
            if source.get("system").is_some() { 3 } else { 2 }
        );
    }
    assert!(request::prepare(br#"{"system":null}"#).is_err());
    assert!(request::prepare(b"[]").is_err());
    assert!(request::prepare(b"bad").is_err());
}

#[tokio::test]
async fn native_request_replaces_credentials_and_keeps_caller_tools() {
    let mock = Arc::new(Mock::default());
    mock.replies
        .lock()
        .unwrap()
        .push_back(response(200, reply()));
    let (core, endpoint, _) = setup(mock.clone());
    let incoming = incoming(
        json!({"model":"glm-5.3-flash","max_tokens":2048,"system":"Host",
        "messages":[{"role":"user","content":"Hi"}], "tools":[{"name":"echo","input_schema":{"type":"object"}}]}),
    );
    let result = core
        .passthrough(
            &endpoint,
            incoming,
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, 200);
    let seen = mock.seen.lock().unwrap();
    assert_eq!(
        seen[0].url,
        "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages"
    );
    assert_eq!(
        header(&seen[0].headers, "authorization"),
        Some(format!("Bearer {}", credentials("one").zcode_jwt).as_str())
    );
    for forbidden in ["cookie", "x-api-key"] {
        assert!(header(&seen[0].headers, forbidden).is_none());
    }
    assert!(
        header(&seen[0].headers, "user-agent")
            .unwrap()
            .starts_with("ZCode/3.17.2")
    );
    let body: Value = serde_json::from_slice(&seen[0].body).unwrap();
    assert_eq!(body["tools"][0]["name"], "echo");
    assert!(body.get("thinking").is_none());
    assert!(body.get("output_config").is_none());
}

#[tokio::test]
async fn wrong_origin_or_route_never_receives_a_token() {
    let mock = Arc::new(Mock::default());
    let (_, _, plugin) = setup(mock.clone());
    let ctx = RequestCtx {
        id: 1,
        format: Format::AnthropicMessages,
        streaming: false,
        attempt: 0,
    };
    for url in [
        "https://other.example/v1/messages",
        "https://zcode.z.ai.evil/api/v1/zcode-plan/anthropic/v1/messages",
        "https://zcode.z.ai/api/v1/zcode-plan/anthropic/v1/messages?redirect=evil",
        "https://zcode.z.ai/api/v1/oauth/token",
    ] {
        let mut request = HttpRequest {
            method: "POST".into(),
            url: url.into(),
            headers: vec![],
            body: Bytes::from_static(b"{}"),
        };
        assert!(plugin.on_request(&ctx, &mut request).await.is_err());
    }
    assert!(mock.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn all_core_formats_translate_json_and_chunked_sse() {
    for client_format in [
        Format::AnthropicMessages,
        Format::OpenAiChat,
        Format::OpenAiResponses,
        Format::Gemini,
    ] {
        for streaming in [false, true] {
            let mock = Arc::new(Mock::default());
            mock.replies.lock().unwrap().push_back(if streaming {
                stream_reply()
            } else {
                response(200, reply())
            });
            let (core, endpoint, _) = setup(mock);
            let (path, body) = match client_format {
                Format::AnthropicMessages => (
                    "/v1/messages".to_owned(),
                    json!({"model":"glm-5.3-flash","max_tokens":2048,"system":"Host","messages":[{"role":"user","content":"Hi"}],"stream":streaming}),
                ),
                Format::OpenAiChat => (
                    "/v1/chat/completions".into(),
                    json!({"model":"glm-5.3-flash","messages":[{"role":"system","content":"Host"},{"role":"user","content":"Hi"}],"stream":streaming}),
                ),
                Format::OpenAiResponses => (
                    "/v1/responses".into(),
                    json!({"model":"glm-5.3-flash","instructions":"Host","input":"Hi","stream":streaming}),
                ),
                Format::Gemini => (
                    format!(
                        "/v1beta/models/glm-5.3-flash:{}",
                        if streaming {
                            "streamGenerateContent?alt=sse"
                        } else {
                            "generateContent"
                        }
                    ),
                    json!({"contents":[{"role":"user","parts":[{"text":"Hi"}]}]}),
                ),
            };
            let result = core
                .translate(
                    &endpoint,
                    InboundRequest {
                        method: "POST".into(),
                        path_and_query: path,
                        headers: vec![],
                        body: Bytes::from(body.to_string()),
                    },
                    TranslateOptions {
                        encode: EncodeOptions {
                            default_max_tokens: Some(2048),
                            ..Default::default()
                        },
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
                    assert!(response.unwrap().content.contains(&Block::text("NABU_OK")))
                }
                TranslateBody::Stream(mut body) => {
                    let mut bytes = Vec::new();
                    while let Some(chunk) = body.next().await {
                        bytes.extend_from_slice(&chunk.unwrap());
                    }
                    assert!(String::from_utf8(bytes).unwrap().contains("NABU_OK"));
                }
            }
        }
    }
}

#[tokio::test]
async fn business_errors_remain_visible_in_json_and_sse() {
    let mock = Arc::new(Mock::default());
    mock.replies
        .lock()
        .unwrap()
        .push_back(response(405, json!({"code":3012,"msg":"blocked"})));
    let (core, endpoint, _) = setup(mock);
    let result = core
        .passthrough(
            &endpoint,
            incoming(json!({"model":"glm-5.3","max_tokens":20,"messages":[]})),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.status, 405);
    let PassthroughBody::Full { bytes, .. } = result.body else {
        panic!()
    };
    let error: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(error["error"]["type"], "permission_error");
    assert_eq!(error["error"]["code"], "3012");
    assert_eq!(error["error"]["message"], "blocked");

    let plugin = ZCodeAcc::new(Arc::new(MemoryStore::new(credentials("one"))), client());
    let mut event = ncore::core::SseParser::new()
        .feed(b"event: error\ndata: {\"code\":3009,\"msg\":\"busy\"}\n\n")
        .pop()
        .unwrap();
    plugin
        .on_stream_event(
            &RequestCtx {
                id: 1,
                format: Format::AnthropicMessages,
                streaming: true,
                attempt: 1,
            },
            &mut event,
        )
        .unwrap();
    assert!(event.data.contains("rate_limit_error"));
    assert!(String::from_utf8_lossy(&event.raw).contains("rate_limit_error"));
}

fn balance() -> Value {
    json!({"plans":[{"user_plan_id":"active","name":"Start","status":"active"}, {"user_plan_id":"old","status":"expired"}],
        "balances":[{"user_plan_id":"active","show_name":"GLM-5.3","remaining_units":0,"capabilities":["model:glm-5.3"]},
            {"user_plan_id":"active","show_name":"GLM-5.3-Flash","capabilities":["model:glm-5.3-flash"]},
            {"user_plan_id":"old","capabilities":["model:retired"]}]})
}

#[tokio::test]
async fn usage_is_coalesced_and_preserves_zero_balances_and_scoped_buckets() {
    let mock = Arc::new(Mock::default());
    mock.replies
        .lock()
        .unwrap()
        .push_back(response(200, json!({"code":0,"data":balance()})));
    let (_, _, plugin) = setup(mock.clone());
    let (a, b) = tokio::join!(plugin.usage(&*mock), plugin.usage(&*mock));
    assert!(!a.unwrap().stale);
    let snapshot = b.unwrap();
    assert_eq!(snapshot.value["balances"][0]["remaining_units"], 0);
    assert_eq!(snapshot.model_ids(), vec!["glm-5.3", "glm-5.3-flash"]);
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
    assert_eq!(
        header(&mock.seen.lock().unwrap()[0].headers, "x-device-mid"),
        Some("test-device")
    );
}

#[test]
fn catalog_drops_time_expired_plans_without_dropping_exhausted_models() {
    let mut value = balance();
    value["plans"][0]["ends_at"] = json!("1");
    let snapshot = UsageSnapshot {
        value,
        fetched_at: 1000,
        stale: false,
        refresh_error: None,
    };
    assert!(snapshot.model_ids().is_empty());
}

#[tokio::test]
async fn changed_credentials_do_not_reuse_old_usage_cache() {
    let mock = Arc::new(Mock::default());
    mock.replies.lock().unwrap().extend([
        response(200, json!({"code":0,"data":balance()})),
        response(200, json!({"code":0,"data":{"plans":[],"balances":[]}})),
    ]);
    let store = Arc::new(MemoryStore::new(credentials("one")));
    let plugin = ZCodeAcc::new(store.clone(), client());
    assert!(!plugin.models(&*mock).await.unwrap().is_empty());
    store.save(&credentials("two")).await.unwrap();
    assert!(plugin.models(&*mock).await.unwrap().is_empty());
    assert_eq!(mock.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn oauth_login_is_consumed_and_never_stores_provider_secrets() {
    let mock = Mock::default();
    mock.replies.lock().unwrap().extend([
        response(200,json!({"code":0,"data":{"flow_id":"flow/one","authorize_url":"https://chat.z.ai/api/oauth/authorize?state=private", "expires_at":u64::MAX/1000,"poll_interval_sec":1}})),
        response(200,json!({"code":0,"data":{"status":"ready","token":credentials("login").zcode_jwt,"user":{"user_id":"user-one","email":"user@example.com","name":"Example"},"zai":{"access_token":"unused-provider-secret"}}})),
    ]);
    let mut login = oauth::start_login(&mock, &client()).await.unwrap();
    assert!(!format!("{login:?}").contains("private"));
    let oauth::LoginPoll::Ready(account) = oauth::poll_login(&mock, &client(), &mut login)
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(account.zcode_jwt, credentials("login").zcode_jwt);
    let user = login.user().unwrap();
    assert_eq!(user.id, "user-one");
    assert_eq!(user.email.as_deref(), Some("user@example.com"));
    assert_eq!(user.name.as_deref(), Some("Example"));
    assert!(!format!("{account:?}").contains("signature"));
    assert!(
        oauth::poll_login(&mock, &client(), &mut login)
            .await
            .is_err()
    );
    let seen = mock.seen.lock().unwrap();
    assert!(seen[1].url.ends_with("flow%2Fone"));
    assert_eq!(
        header(&seen[0].headers, "authorization"),
        header(&seen[1].headers, "authorization")
    );
    assert_ne!(
        header(&seen[0].headers, "authorization"),
        Some(format!("Bearer {}", account.zcode_jwt).as_str())
    );
}

#[tokio::test]
async fn pending_login_respects_the_server_poll_interval() {
    let mock = Mock::default();
    mock.replies.lock().unwrap().extend([
        response(200,json!({"code":0,"data":{"flow_id":"flow","authorize_url":"https://chat.z.ai/auth", "expires_at":u64::MAX/1000,"poll_interval_sec":300}})),
        response(200,json!({"code":0,"data":{"status":"pending"}})),
    ]);
    let mut login = oauth::start_login(&mock, &client()).await.unwrap();
    assert!(matches!(
        oauth::poll_login(&mock, &client(), &mut login)
            .await
            .unwrap(),
        oauth::LoginPoll::Pending
    ));
    assert!(matches!(
        oauth::poll_login(&mock, &client(), &mut login)
            .await
            .unwrap(),
        oauth::LoginPoll::Pending
    ));
    assert_eq!(mock.seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn usage_failure_retains_stale_data_and_honors_retry_after() {
    let mock = Arc::new(Mock::default());
    let mut busy = response(429, json!({"code":3009,"msg":"busy"}));
    busy.headers.push(("retry-after".into(), "120".into()));
    mock.replies
        .lock()
        .unwrap()
        .extend([response(200, json!({"code":0,"data":balance()})), busy]);
    let (_, _, plugin) = setup(mock.clone());
    let good = plugin.usage(&*mock).await.unwrap();
    plugin.invalidate_usage().await;
    let stale = plugin.usage(&*mock).await.unwrap();
    assert!(stale.stale);
    assert_eq!(stale.value, good.value);
    assert_eq!(stale.fetched_at, good.fetched_at);
    assert!(stale.refresh_error.as_ref().unwrap().contains("429"));
    assert!(plugin.usage(&*mock).await.unwrap().stale);
    assert_eq!(mock.seen.lock().unwrap().len(), 2);
}

#[test]
fn merging_system_keeps_the_last_cache_marker_and_rejects_nontext_data() {
    let value: Value = serde_json::from_slice(
        &request::prepare(
            json!({"system":[
        {"type":"text","text":"A","cache_control":{"type":"ephemeral","ttl":"1h"}},
        {"type":"text","text":"B","cache_control":{"type":"ephemeral","ttl":"5m"}},
        {"type":"text","text":"C"}]})
            .to_string()
            .as_bytes(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(value["system"][2]["text"], "A\n\nB\n\nC");
    assert_eq!(value["system"][2]["cache_control"]["ttl"], "5m");
    assert!(
        request::prepare(
            br#"{"system":[{"type":"image","source":{}},{"type":"text","text":"B"}]}"#
        )
        .is_err()
    );
}

#[test]
fn native_encrypted_import_uses_the_host_secret_without_exposing_it() {
    let fixture = include_bytes!("fixtures/native-credentials.json");
    let imported = Credentials::from_zcode_json(fixture, Some("fixture-secret")).unwrap();
    assert_eq!(imported.zcode_jwt, credentials("fixture").zcode_jwt);
    assert!(Credentials::from_zcode_json(fixture, None).is_err());
    assert!(Credentials::from_zcode_json(fixture, Some("wrong-secret")).is_err());
    assert!(!format!("{imported:?}").contains("signature"));
}

#[tokio::test]
async fn core_retry_reloads_credentials_and_regenerates_request_identity() {
    struct Rotate {
        mock: Mock,
        store: Arc<MemoryStore>,
    }
    impl Transport for Rotate {
        fn send(
            &self,
            request: HttpRequest,
        ) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
            boxed(async move {
                let response = self.mock.send(request).await?;
                if response.status == 429 {
                    self.store.save(&credentials("two")).await.unwrap();
                }
                Ok(response)
            })
        }
    }
    let store = Arc::new(MemoryStore::new(credentials("one")));
    let net = Arc::new(Rotate {
        mock: Mock::default(),
        store: store.clone(),
    });
    net.mock.replies.lock().unwrap().extend([
        response(429, json!({"code":3009,"msg":"busy"})),
        response(200, reply()),
    ]);
    let plugin = Arc::new(ZCodeAcc::new(store, client()));
    let core = Core::builder(net.clone())
        .retry(
            ncore::RetryPolicy {
                max_attempts: 2,
                base_delay: std::time::Duration::ZERO,
                ..Default::default()
            },
            Arc::new(ncore_tokio::TokioClock),
        )
        .build();
    let result = core
        .passthrough(
            &ZCodeAcc::endpoint().with_plugin(plugin),
            incoming(json!({"system":"Host","messages":[]})),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.attempts, 2);
    let seen = net.mock.seen.lock().unwrap();
    assert_ne!(
        header(&seen[0].headers, "authorization"),
        header(&seen[1].headers, "authorization")
    );
    assert_ne!(
        header(&seen[0].headers, "x-request-id"),
        header(&seen[1].headers, "x-request-id")
    );
    assert_eq!(seen[0].body, seen[1].body);
    assert_eq!(
        serde_json::from_slice::<Value>(&seen[1].body).unwrap()["system"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn expired_jwt_is_not_sent_or_refreshed() {
    let token = format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(br#"{"exp":1}"#));
    let plugin = Arc::new(ZCodeAcc::new(
        Arc::new(MemoryStore::new(Credentials::new(token).unwrap())),
        client(),
    ));
    let mock = Arc::new(Mock::default());
    let core = Core::builder(mock.clone()).build();
    let result = core
        .passthrough(
            &ZCodeAcc::endpoint().with_plugin(plugin),
            incoming(json!({"messages":[]})),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await;
    assert!(matches!(result, Err(Error::Plugin { .. })));
    assert!(mock.seen.lock().unwrap().is_empty());
    assert!(Credentials::new("not\r\na-token").is_err());
}
