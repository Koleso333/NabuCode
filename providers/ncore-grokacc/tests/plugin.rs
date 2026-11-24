use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use ncore::core::ir::{Block, Message, Request, Role, plain_text};
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    CancelToken, Core, GenerateOptions, HttpRequest, HttpResponse, Transport, TransportError,
};
use ncore_grokacc::{CredentialStore, Credentials, GrokAcc, MemoryStore, oauth};
use serde_json::{Value, json};

const BASE: &str = "https://cli-chat-proxy.grok.com";

#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<HttpResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl Transport for Script {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        boxed(async move {
            self.requests.lock().unwrap().push(req);
            Ok(self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request"))
        })
    }
}

fn reply(status: u16, value: Value) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: Box::pin(stream::iter([Ok(Bytes::from(value.to_string()))])),
    }
}

fn sse() -> HttpResponse {
    let payload = json!({
        "type":"response.completed",
        "response":{
            "id":"resp_1","model":"grok-4.7","status":"completed",
            "output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"42"}]}],
            "usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":80},"output_tokens":4,"total_tokens":104}
        }
    });
    HttpResponse {
        status: 200,
        headers: vec![("content-type".into(), "text/event-stream".into())],
        body: Box::pin(
            stream::iter([Ok(Bytes::from(format!("data: {payload}\r\n\r\n")))])
                .chain(stream::pending()),
        ),
    }
}

fn store() -> Arc<MemoryStore> {
    Arc::new(MemoryStore::new(Credentials {
        access_token: "access".into(),
        refresh_token: Some("refresh".into()),
        expires_at: u64::MAX,
        user_id: "user".into(),
    }))
}

#[tokio::test]
async fn oauth_state_and_refresh_rotation() {
    let net = Script::default();
    let start = oauth::start_login(8181).unwrap();
    assert!(start.url.contains("code_challenge_method=S256"));
    assert!(
        oauth::finish_login(&net, &Default::default(), start, "code", "wrong")
            .await
            .is_err()
    );
    let start = oauth::start_login(8181).unwrap();
    let state = start
        .url
        .split('&')
        .find_map(|part| part.strip_prefix("state="))
        .unwrap()
        .to_owned();
    net.replies.lock().unwrap().extend([
        reply(
            200,
            json!({"access_token":"new","refresh_token":"new-refresh","expires_in":3600}),
        ),
        reply(200, json!({"userId":"user","subscriptionTier":"Free"})),
    ]);
    let (logged_in, profile) =
        oauth::finish_login(&net, &Default::default(), start, "code", &state)
            .await
            .unwrap();
    assert_eq!(profile.subscription_tier.as_deref(), Some("Free"));
    assert_eq!(logged_in.user_id, "user");
    let store = store();
    let plugin = GrokAcc::new(store.clone());
    net.replies.lock().unwrap().extend([
        reply(401, json!({"error":"expired"})),
        reply(
            200,
            json!({"access_token":"rotated","refresh_token":"rotated-refresh","expires_in":3600}),
        ),
        reply(
            200,
            json!({"data":[{"id":"grok-4.7","name":"Grok 4.7","context_window":500000}]}),
        ),
    ]);
    assert_eq!(
        plugin.models(&net).await.unwrap()[0].context_window,
        256_000
    );
    assert_eq!(
        store.load().await.unwrap().refresh_token.as_deref(),
        Some("rotated-refresh")
    );
    let requests = net.requests.lock().unwrap();
    assert_eq!(requests[0].url, oauth::TOKEN_URL);
    assert!(
        std::str::from_utf8(&requests[0].body)
            .unwrap()
            .contains("code_verifier=")
    );
    assert_eq!(requests[3].url, oauth::TOKEN_URL);
    assert_eq!(
        requests[4]
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .unwrap()
            .1,
        "Bearer rotated"
    );
}

#[tokio::test]
async fn responses_tracks_usage_once_and_classifies_free_limit() {
    let net = Arc::new(Script::default());
    net.replies.lock().unwrap().extend([
        sse(),
        reply(429,json!({"code":"subscription:free-usage-exhausted","error":"Usage resets over a rolling 24-hour window — tokens (actual/limit): 620247/600000. Upgrade."})),
    ]);
    let plugin = Arc::new(GrokAcc::new(store()));
    let other = GrokAcc::new(store());
    let endpoint = GrokAcc::endpoint().with_plugin(plugin.clone());
    let core = Core::builder(net.clone()).build();
    let request = Request {
        model: "grok-4.7".into(),
        messages: vec![Message::new(Role::User, vec![Block::text("Hi")])],
        ..Default::default()
    };
    let options = GenerateOptions {
        headers: vec![GrokAcc::cache_key("session-1")],
        ..Default::default()
    };
    let cancel = CancelToken::new();
    let mut result = core
        .generate(&endpoint, &request, &options, &cancel)
        .await
        .unwrap();
    let response = result.take_response().unwrap();
    assert_eq!(plain_text(&response.content), "42");
    assert_eq!(response.usage.cache_read, Some(80));
    assert_eq!(plugin.free_quota().used_tokens, 104);
    assert_eq!(other.free_quota().used_tokens, 0);
    assert!(
        core.generate(&endpoint, &request, &options, &cancel)
            .await
            .is_err()
    );
    assert_eq!(plugin.free_quota().used_tokens, 620247);
    assert_eq!(plugin.free_quota().remaining_percent, 0.0);
    let saved = serde_json::to_vec(&plugin.free_usage()).unwrap();
    let mut restored: ncore_grokacc::FreeUsage = serde_json::from_slice(&saved).unwrap();
    assert_eq!(restored.snapshot(u64::MAX).used_tokens, 0);
    let requests = net.requests.lock().unwrap();
    assert_eq!(requests[0].url, format!("{BASE}/v1/responses"));
    assert!(
        !requests[0]
            .headers
            .iter()
            .any(|(name, _)| name == "x-nabu-grok-cache-key")
    );
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["prompt_cache_key"], "session-1");
    assert_eq!(body["store"], false);
    assert_eq!(body["include"][0], "reasoning.encrypted_content");
}

#[tokio::test]
async fn billing_preserves_unknown_and_paid_fields() {
    let net = Script::default();
    net.replies.lock().unwrap().extend([
        reply(200,json!({"config":{"currentPeriod":{"end":"2026-11-18T00:00:00Z"}}})),
        reply(200,json!({"config":{"creditUsagePercent":42.5,"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-11-18T00:00:00Z"},"prepaidBalance":{"val":900}}})),
    ]);
    let plugin = GrokAcc::new(store());
    assert_eq!(plugin.billing(&net).await.unwrap().used_percent(), None);
    let paid = plugin.billing(&net).await.unwrap();
    assert_eq!(paid.used_percent(), Some(42.5));
    assert_eq!(paid.resets_at(), Some("2026-11-18T00:00:00Z"));
    assert_eq!(paid.prepaid_balance.unwrap().val, 900);
}
