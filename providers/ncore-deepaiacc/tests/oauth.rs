use std::collections::VecDeque;
use std::sync::Mutex;

use bytes::Bytes;
use futures_util::stream;
use ncore::rt::{BoxFuture, boxed};
use ncore::{Error, HttpRequest, HttpResponse, Transport, TransportError};
use ncore_deepaiacc::oauth::{self, LoginStart};
use serde_json::{Value, json};

const KEY: &str = "test-deepai-key-not-real";

#[derive(Default)]
struct Mock {
    requests: Mutex<Vec<HttpRequest>>,
    replies: Mutex<VecDeque<Result<HttpResponse, TransportError>>>,
}

impl Mock {
    fn new(reply: Result<HttpResponse, TransportError>) -> Self {
        Self {
            replies: Mutex::new(VecDeque::from([reply])),
            ..Default::default()
        }
    }

    fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Transport for Mock {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        boxed(async move {
            self.requests.lock().unwrap().push(request);
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected request or retry")
        })
    }
}

fn response(status: u16, body: impl Into<Bytes>) -> HttpResponse {
    HttpResponse {
        status,
        headers: vec![("content-type".into(), "application/json".into())],
        body: Box::pin(stream::iter([Ok(body.into())])),
    }
}

fn json_response(status: u16, value: Value) -> HttpResponse {
    response(status, value.to_string())
}

fn state(login: &LoginStart) -> String {
    login
        .url()
        .split_once('?')
        .unwrap()
        .1
        .split('&')
        .find_map(|part| part.strip_prefix("state="))
        .unwrap()
        .to_owned()
}

fn login() -> (LoginStart, String) {
    let login = oauth::start_login(8181).unwrap();
    let state = state(&login);
    (login, state)
}

fn message(error: &Error) -> &str {
    match error {
        Error::Plugin { name, message } => {
            assert_eq!(name, "deepaiacc");
            message
        }
        _ => panic!("expected a fixed plugin error"),
    }
}

#[test]
fn login_url_has_random_hex_state_and_host_port() {
    let mut states = Vec::new();
    for port in [1, 8181, u16::MAX] {
        let login = oauth::start_login(port).unwrap();
        let state = state(&login);
        assert_eq!(state.len(), 64);
        assert!(
            state
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        );
        assert_eq!(
            login.url(),
            format!("https://api.deepai.org/deepai_code/desktop_auth/?state={state}&port={port}")
        );
        assert!(!states.contains(&state));
        states.push(state);
    }
    let error = oauth::start_login(0).unwrap_err();
    assert_eq!(
        message(&error),
        "DeepAI sign-in requires a bound loopback port"
    );
}

#[test]
fn login_debug_redacts_state_and_url() {
    let (login, state) = login();
    let debug = format!("{login:?}");
    assert!(debug.contains("LoginStart"));
    assert!(!debug.contains(&state));
    assert!(!debug.contains(login.url()));
    assert!(!debug.contains("https://"));
}

#[tokio::test]
async fn mismatched_state_never_sends() {
    let net = Mock::default();
    for index in [0, 31, 63] {
        let (login, mut state) = login();
        state.replace_range(index..index + 1, "g");
        let error = oauth::finish_login(&net, login, &state, "test-code")
            .await
            .unwrap_err();
        assert_eq!(
            message(&error),
            "invalid DeepAI sign-in callback; start sign-in again"
        );
    }
    for change in [0, 1, 2] {
        let (login, mut state) = login();
        match change {
            0 => state.clear(),
            1 => {
                state.pop();
            }
            _ => state.push('a'),
        }
        assert!(
            oauth::finish_login(&net, login, &state, "test-code")
                .await
                .is_err()
        );
    }
    assert_eq!(net.calls(), 0);
}

#[tokio::test]
async fn invalid_code_never_sends() {
    let net = Mock::default();
    for code in [String::new(), "x".repeat(4097), "é".repeat(2049)] {
        let (login, state) = login();
        let error = oauth::finish_login(&net, login, &state, &code)
            .await
            .unwrap_err();
        assert_eq!(
            message(&error),
            "invalid DeepAI sign-in callback; start sign-in again"
        );
    }
    assert_eq!(net.calls(), 0);
}

#[tokio::test]
async fn exchange_sends_only_code_and_unauthenticated_headers() {
    let net = Mock::new(Ok(json_response(
        200,
        json!({"api_key": KEY, "username": "test-user", "is_pro": true}),
    )));
    let (login, state) = login();
    let code = "test-code\"\\\n?&=é";
    let credentials = oauth::finish_login(&net, login, &state, code)
        .await
        .unwrap();
    credentials.validate().unwrap();
    assert_eq!(credentials.api_key, KEY);
    assert!(credentials.check_in.is_none());
    let debug = format!("{credentials:?}");
    for secret in [
        &credentials.api_key,
        &credentials.machine_uuid,
        &credentials.instance,
    ] {
        assert!(!debug.contains(secret));
    }
    let requests = net.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(request.method, "POST");
    assert_eq!(
        request.url,
        "https://api.deepai.org/deepai_code/desktop_auth/exchange/"
    );
    assert_eq!(
        request.headers,
        vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            ("user-agent".to_owned(), "node".to_owned()),
        ]
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&request.body).unwrap(),
        json!({"code": code})
    );
    let body = std::str::from_utf8(&request.body).unwrap();
    for secret in [
        &state,
        &credentials.api_key,
        &credentials.machine_uuid,
        &credentials.instance,
    ] {
        assert!(!body.contains(secret));
    }
}

#[tokio::test]
async fn profile_login_preserves_native_name_and_tier_without_debug_secrets() {
    let net = Mock::new(Ok(json_response(
        200,
        json!({"api_key": KEY, "username": "  test\n-user\u{0000}  ", "is_pro": true}),
    )));
    let (login, state) = login();
    let account = oauth::finish_login_with_profile(&net, login, &state, "test-code")
        .await
        .unwrap();
    assert_eq!(account.credentials.api_key, KEY);
    assert_eq!(account.username.as_deref(), Some("test-user"));
    assert!(account.is_pro);
    assert_eq!(net.calls(), 1);
    let debug = format!("{account:?}");
    for private in [
        KEY,
        "test-user",
        &account.credentials.machine_uuid,
        &account.credentials.instance,
        &state,
    ] {
        assert!(!debug.contains(private));
    }
}

#[tokio::test]
async fn optional_profile_fields_are_bounded_and_do_not_break_login() {
    for (profile, username, is_pro) in [
        (json!({}), None, false),
        (json!({"username": 5, "is_pro": "true"}), None, false),
        (json!({"username": "\n\t "}), None, false),
        (
            json!({"username": "é".repeat(300), "is_pro": true}),
            Some("é".repeat(128)),
            true,
        ),
    ] {
        let mut value = profile;
        value["api_key"] = KEY.into();
        let net = Mock::new(Ok(json_response(200, value)));
        let (login, state) = login();
        let account = oauth::finish_login_with_profile(&net, login, &state, "test-code")
            .await
            .unwrap();
        assert_eq!(account.username, username);
        assert_eq!(account.is_pro, is_pro);
        assert_eq!(net.calls(), 1);
    }
}

#[tokio::test]
async fn profile_login_reuses_state_validation_and_fixed_errors() {
    let net = Mock::default();
    let (login, _) = login();
    assert!(
        oauth::finish_login_with_profile(&net, login, "wrong", "test-code")
            .await
            .is_err()
    );
    assert_eq!(net.calls(), 0);
    let net = Mock::new(Ok(json_response(403, json!({"api_key": KEY}))));
    let (login, state) = self::login();
    let error = oauth::finish_login_with_profile(&net, login, &state, "test-code")
        .await
        .unwrap_err();
    assert_eq!(
        message(&error),
        "DeepAI sign-in was refused; start sign-in again"
    );
    assert!(!error.to_string().contains(KEY));
    assert_eq!(net.calls(), 1);
}

#[tokio::test]
async fn maximum_code_length_is_accepted() {
    let net = Mock::new(Ok(json_response(200, json!({"api_key": KEY}))));
    let (login, state) = login();
    let code = "x".repeat(4096);
    oauth::finish_login(&net, login, &state, &code)
        .await
        .unwrap();
    assert_eq!(net.calls(), 1);
    let requests = net.requests.lock().unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&requests[0].body).unwrap()["code"],
        code
    );
}

#[tokio::test]
async fn fresh_login_changes_identity_but_persistence_keeps_it() {
    let mut accounts = Vec::new();
    for _ in 0..2 {
        let net = Mock::new(Ok(json_response(200, json!({"api_key": KEY}))));
        let (login, state) = login();
        let credentials = oauth::finish_login(&net, login, &state, "test-code")
            .await
            .unwrap();
        credentials.validate().unwrap();
        accounts.push(credentials);
    }
    assert_eq!(accounts[0].account_id(), accounts[1].account_id());
    assert_ne!(accounts[0].machine_uuid, accounts[1].machine_uuid);
    assert_ne!(accounts[0].instance, accounts[1].instance);
    let saved = serde_json::to_vec(&accounts[0]).unwrap();
    let restored: ncore_deepaiacc::Credentials = serde_json::from_slice(&saved).unwrap();
    restored.validate().unwrap();
    assert_eq!(restored, accounts[0]);
}

#[tokio::test]
async fn refused_exchange_has_fixed_errors_and_no_retry() {
    for (status, expected) in [
        (
            400,
            "DeepAI sign-in link expired or was already used; start sign-in again",
        ),
        (
            401,
            "DeepAI sign-in link expired or was already used; start sign-in again",
        ),
        (403, "DeepAI sign-in was refused; start sign-in again"),
        (429, "too many DeepAI sign-in attempts; try again later"),
        (302, "DeepAI could not finish sign-in; try again later"),
        (500, "DeepAI could not finish sign-in; try again later"),
        (0, "DeepAI could not finish sign-in; try again later"),
        (199, "DeepAI could not finish sign-in; try again later"),
        (u16::MAX, "DeepAI could not finish sign-in; try again later"),
    ] {
        let net = Mock::new(Ok(json_response(
            status,
            json!({"api_key": KEY, "error": "PRIVATE_SERVER_MARKER"}),
        )));
        let (login, state) = login();
        let error = oauth::finish_login(&net, login, &state, "private-test-code")
            .await
            .unwrap_err();
        assert_eq!(message(&error), expected);
        for text in [error.to_string(), format!("{error:?}")] {
            for secret in [KEY, "PRIVATE_SERVER_MARKER", "private-test-code", &state] {
                assert!(!text.contains(secret));
            }
        }
        assert_eq!(net.calls(), 1);
    }
}

#[tokio::test]
async fn transport_failure_is_redacted_and_never_retried() {
    for connect in [false, true] {
        let net = Mock::new(Err(TransportError {
            message: "PRIVATE_TRANSPORT_MARKER private-test-code".into(),
            connect,
        }));
        let (login, state) = login();
        let error = oauth::finish_login(&net, login, &state, "private-test-code")
            .await
            .unwrap_err();
        assert_eq!(
            message(&error),
            "could not reach DeepAI to finish sign-in; start sign-in again"
        );
        assert!(!format!("{error:?}").contains("PRIVATE_TRANSPORT_MARKER"));
        assert!(!error.to_string().contains("private-test-code"));
        assert_eq!(net.calls(), 1);
    }
}

#[tokio::test]
async fn invalid_json_and_keys_have_one_fixed_error() {
    let mut bodies = vec![
        "not JSON PRIVATE_SERVER_MARKER".to_owned(),
        String::new(),
        format!("{{\"api_key\":\"{KEY}\","),
    ];
    bodies.extend(
        [
            Value::Null,
            json!([]),
            json!({}),
            json!({"apiKey": KEY}),
            json!({"api_key": null}),
            json!({"api_key": 7}),
            json!({"api_key": false}),
            json!({"api_key": {"value": KEY}}),
            json!({"api_key": ""}),
            json!({"api_key": "\r"}),
            json!({"api_key": "key\nPRIVATE_SERVER_MARKER"}),
            json!({"api_key": "key with spaces"}),
            json!({"api_key": "key\u{0000}"}),
            json!({"api_key": "clé"}),
        ]
        .map(|value| value.to_string()),
    );
    for body in bodies {
        let net = Mock::new(Ok(response(200, body)));
        let (login, state) = login();
        let error = oauth::finish_login(&net, login, &state, "test-code")
            .await
            .unwrap_err();
        assert_eq!(
            message(&error),
            "invalid DeepAI sign-in response; start sign-in again"
        );
        for text in [error.to_string(), format!("{error:?}")] {
            assert!(!text.contains(KEY));
            assert!(!text.contains("PRIVATE_SERVER_MARKER"));
        }
        assert_eq!(net.calls(), 1);
    }
}

#[tokio::test]
async fn empty_success_body_is_invalid() {
    let net = Mock::new(Ok(response(204, Bytes::new())));
    let (login, state) = login();
    let error = oauth::finish_login(&net, login, &state, "test-code")
        .await
        .unwrap_err();
    assert_eq!(
        message(&error),
        "invalid DeepAI sign-in response; start sign-in again"
    );
    assert_eq!(net.calls(), 1);
}

#[tokio::test]
async fn response_body_is_bounded_and_read_failures_are_redacted() {
    let replies = [
        response(
            200,
            json!({"api_key": KEY, "username": "x".repeat(64 * 1024)}).to_string(),
        ),
        HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: Box::pin(stream::iter([Err(TransportError::new(
                "PRIVATE_BODY_MARKER",
            ))])),
        },
    ];
    for reply in replies {
        let net = Mock::new(Ok(reply));
        let (login, state) = login();
        let error = oauth::finish_login(&net, login, &state, "test-code")
            .await
            .unwrap_err();
        assert_eq!(
            message(&error),
            "invalid DeepAI sign-in response; start sign-in again"
        );
        assert!(!format!("{error:?}").contains("PRIVATE_BODY_MARKER"));
        assert_eq!(net.calls(), 1);
    }
}

#[tokio::test]
async fn successful_response_can_arrive_in_chunks() {
    let body = json!({"api_key": KEY}).to_string();
    let chunks: Vec<_> = body
        .as_bytes()
        .chunks(3)
        .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
        .collect();
    let net = Mock::new(Ok(HttpResponse {
        status: 201,
        headers: Vec::new(),
        body: Box::pin(stream::iter(chunks)),
    }));
    let (login, state) = login();
    let credentials = oauth::finish_login(&net, login, &state, "test-code")
        .await
        .unwrap();
    credentials.validate().unwrap();
    assert_eq!(credentials.api_key, KEY);
    assert_eq!(net.calls(), 1);
}
