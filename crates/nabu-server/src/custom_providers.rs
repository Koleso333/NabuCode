use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use nabu_protocol::{
    CustomKeyInput, CustomProvider, CustomProviderConfig, Model, ModelRef, Provider,
    ProviderProtocol, SaveCustomProvider,
};
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    Endpoint, Error, Format, GenerateOptions, HttpRequest, HttpResponse, Middleware, Next,
    RequestCtx,
};
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};

use crate::models::Route;

pub struct CustomProviders {
    path: PathBuf,
    providers: Mutex<Result<BTreeMap<String, StoredProvider>, String>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredProvider {
    config: CustomProviderConfig,
    keys: Vec<StoredKey>,
    #[serde(skip)]
    routing: Arc<Mutex<KeyRouting>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredKey {
    id: String,
    credential: Credential,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
enum Credential {
    Value(String),
    Env(String),
}

impl Credential {
    fn resolve(&self) -> Option<String> {
        match self {
            Self::Value(value) => Some(value.clone()),
            Self::Env(name) => std::env::var(name).ok().filter(|value| !value.is_empty()),
        }
    }
}

impl CustomProviders {
    pub fn load(dir: &Path) -> Self {
        let path = dir.join("custom-providers.json");
        let providers = match std::fs::read(&path) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map_err(|err| format!("Could not read custom providers: {err}")),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(err) => Err(format!("Could not read custom providers: {err}")),
        };
        Self {
            path,
            providers: Mutex::new(providers),
        }
    }

    pub fn get(&self, id: &str) -> Result<CustomProvider, String> {
        let providers = self.providers.lock().unwrap();
        let saved = providers
            .as_ref()
            .map_err(Clone::clone)?
            .get(id)
            .ok_or("Custom provider not found")?;
        Ok(CustomProvider {
            config: saved.config.clone(),
            keys: saved.keys.iter().map(|key| key.id.clone()).collect(),
        })
    }

    pub fn save(&self, input: SaveCustomProvider) -> Result<(), String> {
        let SaveCustomProvider { config, keys, edit } = input;
        validate(&config, !keys.is_empty())?;
        let mut providers = self.providers.lock().unwrap();
        let providers = providers.as_mut().map_err(|err| err.clone())?;
        let previous = providers.get(&config.id);
        match (edit, previous.is_some()) {
            (true, false) => return Err("Custom provider not found".into()),
            (false, true) => return Err("A provider with this ID already exists".into()),
            _ => {}
        }
        let keys = resolve_keys(keys, previous)?;
        let routing = previous
            .filter(|saved| {
                saved.config.protocol == config.protocol
                    && saved.config.base_url == config.base_url
                    && saved.config.headers == config.headers
            })
            .map(|saved| routing_after_edit(saved, &keys, config.smart_routing))
            .unwrap_or_default();
        let mut updated = providers.clone();
        updated.insert(
            config.id.clone(),
            StoredProvider {
                config,
                keys,
                routing,
            },
        );
        self.write(&updated)?;
        *providers = updated;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<(), String> {
        let mut providers = self.providers.lock().unwrap();
        let providers = providers.as_mut().map_err(|err| err.clone())?;
        let mut updated = providers.clone();
        if updated.remove(id).is_none() {
            return Err("Custom provider not found".into());
        }
        self.write(&updated)?;
        *providers = updated;
        Ok(())
    }

    fn write(&self, providers: &BTreeMap<String, StoredProvider>) -> Result<(), String> {
        let body = serde_json::to_vec_pretty(providers).map_err(|err| err.to_string())?;
        let write = || -> std::io::Result<()> {
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = self.path.with_extension("json.tmp");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            use std::io::Write;
            let mut file = options.open(&tmp)?;
            file.write_all(&body)?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(tmp, &self.path)
        };
        write().map_err(|err| format!("Could not save custom providers: {err}"))
    }

    pub fn catalog(&self) -> Vec<Provider> {
        let providers = self.providers.lock().unwrap();
        let Ok(providers) = providers.as_ref() else {
            return Vec::new();
        };
        providers
            .values()
            .map(|saved| {
                let config = &saved.config;
                Provider {
                    id: config.id.clone(),
                    name: config.name.clone(),
                    custom: true,
                    models: config
                        .models
                        .iter()
                        .map(|model| Model {
                            id: model.id.clone(),
                            name: [model.family.as_str(), model.version.as_str()]
                                .into_iter()
                                .filter(|part| !part.is_empty())
                                .collect::<Vec<_>>()
                                .join(" "),
                            family: model.family.to_lowercase(),
                            efforts: Vec::new(),
                            context: 0,
                            output: 0,
                            images: false,
                            fast: false,
                            older: false,
                        })
                        .collect(),
                }
            })
            .collect()
    }

    pub fn route(&self, session: &str, model: &ModelRef) -> Result<Route, String> {
        let providers = self.providers.lock().unwrap();
        let saved = providers
            .as_ref()
            .map_err(Clone::clone)?
            .get(&model.provider_id)
            .ok_or_else(|| format!("Provider {} is not supported yet", model.provider_id))?;
        if !saved
            .config
            .models
            .iter()
            .any(|entry| entry.id == model.model_id)
        {
            return Err(format!(
                "Model {} is not configured for {}",
                model.model_id, saved.config.name
            ));
        }

        let config = &saved.config;
        let (format, version) = match config.protocol {
            ProviderProtocol::OpenAiChat => (Format::OpenAiChat, "/v1"),
            ProviderProtocol::OpenAiResponses => (Format::OpenAiResponses, "/v1"),
            ProviderProtocol::AnthropicMessages => (Format::AnthropicMessages, "/v1"),
            ProviderProtocol::Gemini => (Format::Gemini, "/v1beta"),
        };
        let url = reqwest::Url::parse(&config.base_url).map_err(|_| "Invalid provider base URL")?;
        let base_url = url.as_str().trim_end_matches('/');
        let mut endpoint = Endpoint::new(format, base_url).with_signature_namespace(&config.id);
        endpoint.headers = config.headers.clone().into_iter().collect();
        let path_override =
            (!url.path().trim_matches('/').is_empty()).then(|| (base_url.to_owned(), version));
        endpoint = endpoint.with_middleware(Arc::new(ProviderMiddleware {
            provider: saved.clone(),
            session: session.into(),
            path_override,
        }));
        Ok(Route {
            endpoint,
            options: GenerateOptions::default(),
            account: None,
        })
    }
}

fn validate(config: &CustomProviderConfig, has_keys: bool) -> Result<(), String> {
    let id = config.id.as_str();
    if id.is_empty()
        || id.starts_with('-')
        || id.ends_with('-')
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || matches!(
            id,
            "antigravity"
                | "claude-account"
                | "chatgpt-account"
                | "zcode-account"
                | "workbuddy-account"
                | "agentrouter"
                | "coarena"
                | "clauden"
                | "gemini"
        )
    {
        return Err("Invalid or reserved provider ID".into());
    }
    if config.name.trim().is_empty() || config.models.is_empty() {
        return Err("Enter a provider name and at least one model".into());
    }
    let url = reqwest::Url::parse(&config.base_url).map_err(|_| "Invalid provider base URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Use an HTTP(S) base URL without credentials, query or fragment".into());
    }
    let mut models = HashSet::new();
    for model in &config.models {
        if model.id.trim().is_empty() || model.family.trim().is_empty() || !models.insert(&model.id)
        {
            return Err("Enter a unique ID and family for each model".into());
        }
    }
    let mut headers = HashSet::new();
    for (name, value) in &config.headers {
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(|_| "Invalid HTTP header name")?;
        HeaderValue::from_str(value).map_err(|_| "Invalid HTTP header value")?;
        if has_keys && (name == auth_header(config.protocol) || name == "authorization") {
            return Err("The authorization header conflicts with the API keys".into());
        }
        if !headers.insert(name) {
            return Err("Duplicate HTTP header name".into());
        }
    }
    Ok(())
}

fn resolve_keys(
    inputs: Vec<CustomKeyInput>,
    previous: Option<&StoredProvider>,
) -> Result<Vec<StoredKey>, String> {
    let mut keys = Vec::<StoredKey>::new();
    for input in inputs {
        let credential = match input {
            CustomKeyInput::Keep(id) => {
                let saved = previous
                    .and_then(|saved| saved.keys.iter().find(|key| key.id == id))
                    .ok_or("A saved API key changed. Reopen the provider settings.")?;
                if !keys.iter().any(|key| key.credential == saved.credential) {
                    keys.push(saved.clone());
                }
                continue;
            }
            CustomKeyInput::Value(value) => {
                if value.is_empty() || HeaderValue::from_str(&value).is_err() {
                    return Err("Invalid API key".into());
                }
                Credential::Value(value)
            }
            CustomKeyInput::Env(name) => {
                if name.is_empty() || name.contains(['=', '\0']) {
                    return Err("Invalid API key environment variable name".into());
                }
                Credential::Env(name)
            }
        };
        if !keys.iter().any(|key| key.credential == credential) {
            let mut id = [0; 16];
            getrandom::fill(&mut id).map_err(|err| err.to_string())?;
            keys.push(StoredKey {
                id: format!("{:032x}", u128::from_ne_bytes(id)),
                credential,
            });
        }
    }
    Ok(keys)
}

fn auth_header(protocol: ProviderProtocol) -> &'static str {
    match protocol {
        ProviderProtocol::OpenAiChat | ProviderProtocol::OpenAiResponses => "authorization",
        ProviderProtocol::AnthropicMessages => "x-api-key",
        ProviderProtocol::Gemini => "x-goog-api-key",
    }
}

#[derive(Default)]
struct KeyRouting {
    sessions: HashMap<String, usize>,
    active: usize,
    next: usize,
    unavailable: HashMap<usize, Unavailable>,
}

#[derive(Clone, Copy)]
enum Unavailable {
    Until(Instant),
    Rejected,
}

fn routing_after_edit(
    saved: &StoredProvider,
    keys: &[StoredKey],
    smart: bool,
) -> Arc<Mutex<KeyRouting>> {
    let same_mode = saved.config.smart_routing == smart;
    if same_mode
        && saved
            .keys
            .iter()
            .map(|key| &key.id)
            .eq(keys.iter().map(|key| &key.id))
    {
        return saved.routing.clone();
    }

    let previous = saved.routing.lock().unwrap();
    let new_index = |index| {
        let old: &StoredKey = saved.keys.get(index)?;
        keys.iter().position(|key| key.id == old.id)
    };
    Arc::new(Mutex::new(KeyRouting {
        active: if same_mode {
            new_index(previous.active).unwrap_or_default()
        } else {
            0
        },
        next: if same_mode {
            new_index(previous.next).unwrap_or_default()
        } else {
            0
        },
        sessions: if same_mode && smart {
            previous
                .sessions
                .iter()
                .filter_map(|(session, &index)| Some((session.clone(), new_index(index)?)))
                .collect()
        } else {
            HashMap::new()
        },
        unavailable: previous
            .unavailable
            .iter()
            .filter_map(|(&index, &state)| Some((new_index(index)?, state)))
            .collect(),
    }))
}

struct ProviderMiddleware {
    provider: StoredProvider,
    session: String,
    path_override: Option<(String, &'static str)>,
}

impl ProviderMiddleware {
    fn choose(&self, tried: &HashSet<usize>) -> Option<(usize, String)> {
        let mut routing = self.provider.routing.lock().unwrap();
        let keys = &self.provider.keys;
        let smart = self.provider.config.smart_routing;
        let pinned = routing.sessions.get(&self.session).copied();
        let start = if smart {
            pinned.unwrap_or(routing.next)
        } else {
            routing.active
        };
        let now = Instant::now();
        let (index, value) = (0..keys.len()).find_map(|offset| {
            let index = (start + offset) % keys.len();
            if tried.contains(&index)
                || match routing.unavailable.get(&index) {
                    Some(Unavailable::Until(until)) => now < *until,
                    Some(Unavailable::Rejected) => true,
                    None => false,
                }
            {
                return None;
            }
            let value = keys[index].credential.resolve()?;
            HeaderValue::from_str(&value).ok()?;
            Some((index, value))
        })?;
        if smart {
            if pinned != Some(index) {
                routing.next = (index + 1) % keys.len();
                routing.sessions.insert(self.session.clone(), index);
            }
        } else {
            routing.active = index;
        }
        Some((index, value))
    }
}

impl Middleware for ProviderMiddleware {
    fn name(&self) -> &str {
        "custom provider"
    }

    fn handle<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        mut req: HttpRequest,
        next: Next<'a>,
    ) -> BoxFuture<'a, Result<HttpResponse, Error>> {
        boxed(async move {
            if let Some((base, version)) = &self.path_override
                && let Some(path) = req
                    .url
                    .strip_prefix(base)
                    .and_then(|path| path.strip_prefix(version))
            {
                req.url = format!("{base}{path}");
            }
            if self.provider.keys.is_empty() {
                return next.run(req).await;
            }
            let name = auth_header(self.provider.config.protocol);
            let mut tried = HashSet::new();
            let mut last = None;
            while let Some((index, key)) = self.choose(&tried) {
                let mut attempt = req.clone();
                attempt
                    .headers
                    .retain(|(header, _)| !header.eq_ignore_ascii_case(name));
                let value = if name == "authorization" {
                    format!("Bearer {key}")
                } else {
                    key
                };
                attempt.headers.push((name.into(), value));
                let response = next.run(attempt).await?;
                let unavailable = match response.status {
                    401 | 403 => Unavailable::Rejected,
                    402 | 429 => Unavailable::Until(retry_at(response.header("retry-after"))),
                    _ => return Ok(response),
                };
                self.provider
                    .routing
                    .lock()
                    .unwrap()
                    .unavailable
                    .insert(index, unavailable);
                tried.insert(index);
                last = Some(response.status);
            }
            Err(Error::Config(match last {
                Some(status) => format!("No API key accepted the request (HTTP {status})"),
                None => "No API key is available for this custom provider".into(),
            }))
        })
    }
}

fn retry_at(header: Option<&str>) -> Instant {
    let delay = header.and_then(|value| {
        let value = value.trim();
        if let Ok(seconds) = value.parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }
        let date = chrono::DateTime::parse_from_rfc2822(value).ok()?;
        Some(
            SystemTime::from(date)
                .duration_since(SystemTime::now())
                .unwrap_or_default(),
        )
    });
    let now = Instant::now();
    let fallback = Duration::from_secs(5 * 60);
    now.checked_add(delay.unwrap_or(fallback))
        .unwrap_or(now + fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::{Json, Router, routing::post};
    use futures_util::StreamExt;
    use nabu_protocol::{ClientMessage, Reply, Request};
    use ncore::core::ir::{self, Block, Delta, Message, Role, StreamEvent};
    use ncore::{CancelToken, Core};
    use ncore_tokio::HyperTransport;
    use serde_json::{Value, json};

    #[derive(Default)]
    struct Upstream {
        calls: Vec<String>,
        failures: HashMap<String, (StatusCode, &'static str)>,
    }

    async fn completion(
        State(upstream): State<Arc<Mutex<Upstream>>>,
        headers: HeaderMap,
        Json(input): Json<Value>,
    ) -> Response {
        assert_eq!(input["model"], "example-model");
        assert_eq!(headers["x-test"], "custom header");
        let key = headers["authorization"]
            .to_str()
            .unwrap()
            .strip_prefix("Bearer ")
            .unwrap();
        let mut upstream = upstream.lock().unwrap();
        upstream.calls.push(key.into());
        if let Some(&(status, delay)) = upstream.failures.get(key) {
            return (
                status,
                [("retry-after", delay)],
                Json(json!({"error": {"message": "test failure"}})),
            )
                .into_response();
        }
        assert_eq!(input["stream"], true);
        ([("content-type", "text/event-stream")], concat!(
            "data: {\"id\":\"completion\",\"model\":\"example-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"OK\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"completion\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n"
        )).into_response()
    }

    #[tokio::test]
    async fn saves_lists_edits_and_routes_through_a_local_provider() {
        let mut suffix = [0; 8];
        getrandom::fill(&mut suffix).unwrap();
        let dir =
            std::env::temp_dir().join(format!("nabu-custom-{:x}", u64::from_ne_bytes(suffix)));
        let upstream = Arc::new(Mutex::new(Upstream::default()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/proxy/v2", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/proxy/v2/chat/completions", post(completion))
            .with_state(upstream.clone());
        let http = tokio::spawn(async { axum::serve(listener, app).await.unwrap() });

        let server = crate::Server::new(&dir);
        let request: ClientMessage = serde_json::from_value(json!({
            "id": 1, "method": "saveCustomProvider", "params": {
                "id": "local-provider", "name": "Local provider", "protocol": "openAiChat",
                "baseUrl": url, "smartRouting": true,
                "models": [{"id": "example-model", "family": "GPT", "developer": "OpenAI", "version": "6 Astra"}],
                "headers": {"X-Test": "custom header"},
                "keys": [{"type": "value", "value": "test-one"}, {"type": "value", "value": "test-two"}],
                "edit": false,
            }
        })).unwrap();
        let mut events = server.subscribe();
        server.handle(request.request).await.unwrap();
        assert!(matches!(
            events.try_recv(),
            Ok(nabu_protocol::Event::ProvidersChanged)
        ));
        server.start().await;
        let Reply::Providers(providers) = server.handle(Request::Providers).await.unwrap() else {
            panic!("catalog reply")
        };
        assert_eq!(providers[0].models[0].name, "GPT 6 Astra");
        assert!(providers[0].custom);
        let saved = CustomProviders::load(&dir).get("local-provider").unwrap();
        let saved_json = serde_json::to_string(&saved).unwrap();
        assert!(!saved_json.contains("test-one") && !saved_json.contains("test-two"));
        let model = ModelRef {
            provider_id: saved.config.id.clone(),
            model_id: "example-model".into(),
            variant: None,
            fast: false,
        };
        let core = Core::builder(Arc::new(HyperTransport::new())).build();
        let send = |session| {
            let mut request = ir::Request {
                model: model.model_id.clone(),
                messages: vec![Message::new(Role::User, vec![Block::text("Hello")])],
                stream: true,
                ..Default::default()
            };
            let route = server.models.route(session, &model, &mut request).unwrap();
            let core = &core;
            async move {
                let mut stream = core
                    .generate(
                        &route.endpoint,
                        &request,
                        &route.options,
                        &CancelToken::new(),
                    )
                    .await
                    .unwrap();
                let mut text = String::new();
                while let Some(event) = stream.next().await {
                    if let StreamEvent::Delta {
                        delta: Delta::Text(part),
                        ..
                    } = event.unwrap()
                    {
                        text.push_str(&part);
                    }
                }
                assert_eq!(text, "OK");
            }
        };
        send("first").await;
        send("second").await;
        send("first").await;
        assert_eq!(
            std::mem::take(&mut upstream.lock().unwrap().calls),
            ["test-one", "test-two", "test-one"]
        );

        let mut config = saved.config;
        config.smart_routing = false;
        server
            .handle(Request::SaveCustomProvider(SaveCustomProvider {
                config,
                keys: saved.keys.into_iter().map(CustomKeyInput::Keep).collect(),
                edit: true,
            }))
            .await
            .unwrap();
        send("third").await;
        send("fourth").await;
        upstream
            .lock()
            .unwrap()
            .failures
            .insert("test-one".into(), (StatusCode::TOO_MANY_REQUESTS, "0"));
        send("third").await;
        upstream.lock().unwrap().failures.remove("test-one");
        send("fourth").await;
        upstream
            .lock()
            .unwrap()
            .failures
            .insert("test-two".into(), (StatusCode::UNAUTHORIZED, "0"));
        send("fourth").await;
        send("third").await;
        assert_eq!(
            std::mem::take(&mut upstream.lock().unwrap().calls),
            [
                "test-one", "test-one", "test-one", "test-two", "test-two", "test-two", "test-one",
                "test-one"
            ],
        );

        server
            .handle(Request::RemoveCustomProvider {
                id: model.provider_id.clone(),
            })
            .await
            .unwrap();
        assert!(server.models.providers().is_empty());
        assert!(CustomProviders::load(&dir).get(&model.provider_id).is_err());
        http.abort();
        std::fs::remove_file(dir.join("custom-providers.json")).unwrap();
        let _ = std::fs::remove_dir(dir);
    }
}
