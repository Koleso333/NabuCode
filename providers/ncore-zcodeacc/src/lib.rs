mod api;
pub mod credentials;
pub mod oauth;
pub mod quota;
pub mod request;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::lock::Mutex;
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    Endpoint, Error, EventAction, Format, Hook, HttpRequest, HttpResponse, Interests, Middleware,
    Next, RequestCtx, Transport,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub use api::ClientInfo;
use api::{CallError, MESSAGES_URL, escape, fail, json_request, json_response};
pub use credentials::{CredentialStore, Credentials, MemoryStore};
pub use quota::UsageSnapshot;

#[derive(Default)]
struct UsageCache {
    account: Vec<u8>,
    snapshot: Option<UsageSnapshot>,
    next: Option<Instant>,
}

/// Reuse one instance per account. Network policy and timeouts belong to the host transport.
pub struct ZCodeAcc {
    store: Arc<dyn CredentialStore>,
    client: ClientInfo,
    usage: Mutex<UsageCache>,
}

impl ZCodeAcc {
    pub fn new(store: Arc<dyn CredentialStore>, client: ClientInfo) -> Self {
        Self {
            store,
            client,
            usage: Mutex::default(),
        }
    }

    pub fn endpoint() -> Endpoint {
        Endpoint::new(
            Format::AnthropicMessages,
            "https://zcode.z.ai/api/v1/zcode-plan/anthropic",
        )
        .with_signature_namespace("zcodeacc")
    }

    async fn credentials(&self) -> Result<Credentials, Error> {
        let credentials = self
            .store
            .load()
            .await
            .map_err(|_| fail("could not load ZCode credentials"))?;
        credentials.validate().map_err(fail)?;
        self.client.validate()?;
        Ok(credentials)
    }

    async fn get(
        &self,
        transport: &dyn Transport,
        path: &str,
        credentials: &Credentials,
    ) -> Result<Value, CallError> {
        let mut request = json_request("GET", path, &self.client, None);
        request.headers.push((
            "authorization".into(),
            format!("Bearer {}", credentials.zcode_jwt),
        ));
        json_response(transport.send(request).await.map_err(Error::from)?).await
    }

    /// Plan and promo balances; coalesced for 60 seconds, stale data retained on transient failures.
    pub async fn usage(&self, transport: &dyn Transport) -> Result<UsageSnapshot, Error> {
        let credentials = self.credentials().await?;
        let account = Sha256::digest(credentials.zcode_jwt.as_bytes()).to_vec();
        let mut cache = self.usage.lock().await;
        if cache.account != account {
            *cache = UsageCache {
                account,
                ..Default::default()
            };
        }
        if cache.next.is_some_and(|next| next > Instant::now()) {
            return cache
                .snapshot
                .clone()
                .ok_or_else(|| fail("ZCode usage polling is cooling down"));
        }
        let path = format!(
            "/api/v1/zcode-plan/billing/balance?app_version={}",
            escape(&self.client.version)
        );
        let result = self
            .get(transport, &path, &credentials)
            .await
            .and_then(|value| {
                if value["plans"].is_array() && value["balances"].is_array() {
                    Ok(value)
                } else {
                    Err(fail("invalid ZCode balance response").into())
                }
            });
        match result {
            Ok(value) => {
                let snapshot = UsageSnapshot {
                    value,
                    fetched_at: credentials::unix_ms(),
                    stale: false,
                    refresh_error: None,
                };
                cache.snapshot = Some(snapshot.clone());
                cache.next = Some(Instant::now() + Duration::from_secs(60));
                Ok(snapshot)
            }
            Err(error) => {
                cache.next = Some(Instant::now() + error.delay);
                if let Some(snapshot) = cache.snapshot.as_mut() {
                    snapshot.stale = true;
                    snapshot.refresh_error = Some(error.error.to_string());
                    Ok(snapshot.clone())
                } else {
                    Err(error.error)
                }
            }
        }
    }

    pub async fn models(&self, transport: &dyn Transport) -> Result<Vec<String>, Error> {
        Ok(self.usage(transport).await?.model_ids())
    }

    /// Lets the next usage call refresh while retaining the last snapshot on failure.
    pub async fn invalidate_usage(&self) {
        self.usage.lock().await.next = None;
    }

    /// Native server policy, including captcha skip_model_request. No browser solver is spawned.
    pub async fn client_config(&self, transport: &dyn Transport) -> Result<Value, Error> {
        let credentials = self.credentials().await?;
        let path = format!(
            "/api/v1/client/configs?app_version={}&platform={}",
            escape(&self.client.version),
            escape(&self.client.platform)
        );
        self.get(transport, &path, &credentials)
            .await
            .map_err(|e| e.error)
    }
}

fn validate_target(request: &HttpRequest) -> Result<(), Error> {
    if request.method != "POST" || request.url != MESSAGES_URL {
        return Err(fail(
            "ZCodeAcc supports only the native Start Plan Messages endpoint",
        ));
    }
    Ok(())
}

fn authenticate(
    request: &mut HttpRequest,
    credentials: &Credentials,
    client: &ClientInfo,
) -> Result<(), Error> {
    // Rebuild credentials and client identity on every attempt; caller credentials never reach upstream.
    let mut headers = client.headers(true);
    request.headers.retain(|(name, _)| {
        let name = name.to_ascii_lowercase();
        matches!(
            name.as_str(),
            "anthropic-beta" | "x-aliyun-captcha-verify-param" | "x-aliyun-captcha-verify-region"
        )
    });
    for header in headers.drain(..) {
        request.headers.push(header);
    }
    request.headers.push((
        "authorization".into(),
        format!("Bearer {}", credentials.zcode_jwt),
    ));
    let mut nonce = [0; 16];
    getrandom::fill(&mut nonce).map_err(|_| fail("system randomness unavailable"))?;
    nonce[6] = (nonce[6] & 0x0f) | 0x40;
    nonce[8] = (nonce[8] & 0x3f) | 0x80;
    let id: String = nonce.iter().map(|n| format!("{n:02x}")).collect();
    let id = format!(
        "{}-{}-{}-{}-{}",
        &id[..8],
        &id[8..12],
        &id[12..16],
        &id[16..20],
        &id[20..]
    );
    request.headers.push(("x-request-id".into(), id.clone()));
    request.headers.push(("x-zcode-trace-id".into(), id));
    Ok(())
}

impl Hook for ZCodeAcc {
    fn name(&self) -> &str {
        "zcodeacc"
    }
    fn interests(&self) -> Interests {
        Interests {
            request: true,
            body: true,
            stream: true,
            ..Interests::NONE
        }
    }
    fn on_request<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        request: &'a mut HttpRequest,
    ) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            if ctx.format != Format::AnthropicMessages {
                return Err("ZCodeAcc requires Anthropic Messages".into());
            }
            validate_target(request).map_err(|e| e.to_string())?;
            request.body = request::prepare(&request.body)?;
            Ok(())
        })
    }
    fn on_body(&self, _ctx: &RequestCtx, status: u16, body: &mut Bytes) -> Result<(), String> {
        if status >= 400 {
            request::normalize_error(body);
        }
        Ok(())
    }
    fn on_stream_event(
        &self,
        _ctx: &RequestCtx,
        event: &mut ncore::core::SseEvent,
    ) -> Result<EventAction, String> {
        if event.event.as_deref() == Some("error")
            || serde_json::from_str::<Value>(&event.data)
                .ok()
                .is_some_and(|v| v["type"] == "error")
        {
            let mut data = Bytes::copy_from_slice(event.data.as_bytes());
            request::normalize_error(&mut data);
            let normalized =
                String::from_utf8(data.to_vec()).map_err(|_| "invalid error event UTF-8")?;
            event.set_data(&normalized);
        }
        Ok(EventAction::Keep)
    }
}

impl Middleware for ZCodeAcc {
    fn name(&self) -> &str {
        "zcodeacc"
    }
    fn handle<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        mut request: HttpRequest,
        next: Next<'a>,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        boxed(async move {
            validate_target(&request)?;
            let credentials = self.credentials().await?;
            authenticate(&mut request, &credentials, &self.client)?;
            next.run(request).await
        })
    }
}
