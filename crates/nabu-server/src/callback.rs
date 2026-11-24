use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

const TIMEOUT: Duration = Duration::from_secs(180);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const HEAD_LIMIT: usize = 16384;
const CODE_LIMIT: usize = 4096;

/// A local page the browser is sent back to after an OAuth sign-in.
pub struct Callback {
    listeners: Vec<TcpListener>,
    path: &'static str,
    pub redirect: String,
    pub port: u16,
}

impl Callback {
    /// Binds 127.0.0.1 and, when free, ::1 on the same port: browsers resolve localhost to either.
    pub async fn bind(ports: &[u16], path: &'static str) -> std::io::Result<Self> {
        let mut last = None;
        for &port in ports {
            match TcpListener::bind(("127.0.0.1", port)).await {
                Ok(v4) => {
                    let port = v4.local_addr()?.port();
                    let mut listeners = vec![v4];
                    listeners.extend(TcpListener::bind(("::1", port)).await.ok());
                    let redirect = format!("http://localhost:{port}{path}");
                    return Ok(Self {
                        listeners,
                        path,
                        redirect,
                        port,
                    });
                }
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| std::io::Error::other("no free OAuth callback port")))
    }

    /// Waits for the browser redirect carrying a code for `state`.
    pub async fn code(self, state: String) -> Result<String, String> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut tasks = JoinSet::new();
        for listener in self.listeners {
            tasks.spawn(receive(listener, self.path, state.clone(), tx.clone()));
        }
        drop(tx);
        let result = tokio::time::timeout(TIMEOUT, rx.recv()).await;
        tasks.shutdown().await;
        match result {
            Ok(Some(result)) => result,
            _ => Err("sign-in timed out".into()),
        }
    }
}

async fn receive(
    listener: TcpListener,
    path: &'static str,
    state: String,
    tx: mpsc::UnboundedSender<Result<String, String>>,
) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                let state = state.clone();
                // Idle browser preconnects must not block the redirect or outlive this listener.
                connections.spawn(async move { answer(stream, path, &state).await });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Ok(Some(result)) = result {
                    connections.shutdown().await;
                    let _ = tx.send(result);
                    return;
                }
            }
        }
    }
    connections.shutdown().await;
}

#[derive(Debug, PartialEq, Eq)]
enum Reply {
    NotFound,
    Invalid,
    Refused,
    Code(String),
}

async fn answer(mut stream: TcpStream, path: &str, state: &str) -> Option<Result<String, String>> {
    let reply = match read_head(&mut stream).await {
        Some(head) => validate(&head, path, state),
        None => Reply::Invalid,
    };
    let (status, message) = match &reply {
        Reply::NotFound => ("404 Not Found", ""),
        Reply::Invalid => ("400 Bad Request", "Invalid sign-in callback."),
        Reply::Refused => (
            "400 Bad Request",
            "Sign-in was refused. You can close this tab and return to NabuCode.",
        ),
        Reply::Code(_) => (
            "200 OK",
            "Signed in. You can close this tab and return to NabuCode.",
        ),
    };
    let html = if message.is_empty() {
        String::new()
    } else {
        format!(
            "<html><body style='font-family:system-ui;padding:40px;text-align:center'><h2>{message}</h2></body></html>"
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{html}",
        html.len()
    );
    let _ = tokio::time::timeout(READ_TIMEOUT, stream.write_all(response.as_bytes())).await;
    let _ = stream.shutdown().await;
    match reply {
        Reply::Code(code) => Some(Ok(code)),
        Reply::Refused => Some(Err("the sign-in was refused".into())),
        Reply::NotFound | Reply::Invalid => None,
    }
}

async fn read_head(stream: &mut TcpStream) -> Option<Vec<u8>> {
    tokio::time::timeout(READ_TIMEOUT, async {
        let mut head = Vec::new();
        let mut chunk = [0u8; 2048];
        loop {
            let remaining = HEAD_LIMIT - head.len();
            if remaining == 0 {
                return None;
            }
            let size = remaining.min(chunk.len());
            let n = stream.read(&mut chunk[..size]).await.ok()?;
            if n == 0 {
                return None;
            }
            head.extend_from_slice(&chunk[..n]);
            if let Some(end) = head.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                head.truncate(end + 4);
                return Some(head);
            }
        }
    })
    .await
    .ok()
    .flatten()
}

fn validate(head: &[u8], path_wanted: &str, state: &str) -> Reply {
    let Some((method, target)) = parse_head(head) else {
        return Reply::Invalid;
    };
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != path_wanted {
        return Reply::NotFound;
    }
    if method != "GET" {
        return Reply::Invalid;
    }
    query_reply(query, state)
}

fn parse_head(head: &[u8]) -> Option<(&str, &str)> {
    if head.len() > HEAD_LIMIT {
        return None;
    }
    let head = std::str::from_utf8(head).ok()?.strip_suffix("\r\n\r\n")?;
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if method.is_empty()
        || !method.bytes().all(is_token)
        || !matches!(version, "HTTP/1.0" | "HTTP/1.1")
        || parts.next().is_some()
    {
        return None;
    }
    if !target.starts_with('/')
        || target
            .bytes()
            .any(|byte| byte <= b' ' || byte >= 127 || byte == b'#')
    {
        return None;
    }
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.is_empty() || !name.bytes().all(is_token) {
            return None;
        }
        if value
            .bytes()
            .any(|byte| (byte < b' ' && byte != b'\t') || byte == 127)
        {
            return None;
        }
    }
    Some((method, target))
}

fn is_token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

fn query_reply(query: &str, state: &str) -> Reply {
    let (mut actual_state, mut code, mut error) = (None, None, false);
    for field in query.split('&') {
        let (key, value) = field.split_once('=').unwrap_or((field, ""));
        let (Some(key), Some(value)) = (percent_decode(key), percent_decode(value)) else {
            return Reply::Invalid;
        };
        match key.as_str() {
            "state" => {
                if actual_state.replace(value).is_some() {
                    return Reply::Invalid;
                }
            }
            "code" => {
                if code.replace(value).is_some() {
                    return Reply::Invalid;
                }
            }
            "error" => {
                if error || value.is_empty() {
                    return Reply::Invalid;
                }
                error = true;
            }
            _ => {}
        }
    }
    if actual_state.as_deref() != Some(state) {
        return Reply::Invalid;
    }
    match (code, error) {
        (Some(code), false) if !code.is_empty() && code.len() <= CODE_LIMIT => Reply::Code(code),
        (None, true) => Reply::Refused,
        _ => Reply::Invalid,
    }
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                if !hex.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
                i += 3;
            }
            byte => {
                out.push(if byte == b'+' { b' ' } else { byte });
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    const PATH: &str = "/auth-callback";
    const STATE: &str = "expected-state";
    const TEST_TIMEOUT: Duration = Duration::from_secs(5);

    fn head(target: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n")
    }

    async fn exchange(address: SocketAddr, request: &str) -> String {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let mut stream = TcpStream::connect(address).await.unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            let mut response = Vec::new();
            let result = stream.read_to_end(&mut response).await;
            if let Err(error) = result {
                assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
                assert!(!response.is_empty());
            }
            String::from_utf8(response).unwrap()
        })
        .await
        .expect("local callback did not respond")
    }

    async fn assert_closed(mut stream: TcpStream) {
        let mut byte = [0];
        match tokio::time::timeout(TEST_TIMEOUT, stream.read(&mut byte))
            .await
            .expect("connection worker survived the callback")
        {
            Ok(n) => assert_eq!(n, 0),
            Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset),
        }
    }

    #[test]
    fn accepts_get_and_decodes_query() {
        let request =
            head("/auth-callback?state=known+state%2F%2B&code=c%2B%26%3D%E2%9C%93&scope=openid");
        assert_eq!(
            validate(request.as_bytes(), PATH, "known state/+"),
            Reply::Code("c+&=✓".into())
        );
        let request =
            head("/auth-callback?state=expected-state&code=ok").replace("HTTP/1.1", "HTTP/1.0");
        assert_eq!(
            validate(request.as_bytes(), PATH, STATE),
            Reply::Code("ok".into())
        );
    }

    #[test]
    fn rejects_invalid_request_lines_and_headers() {
        for request in [
            "POST /auth-callback?state=expected-state&code=ok HTTP/1.1\r\n\r\n",
            "HEAD /auth-callback?state=expected-state&code=ok HTTP/1.1\r\n\r\n",
            "GET /auth-callback?state=expected-state&code=ok HTTP/2.0\r\n\r\n",
            "GET /auth-callback?state=expected-state&code=ok HTTP/1.1 extra\r\n\r\n",
            "GET  /auth-callback?state=expected-state&code=ok HTTP/1.1\r\n\r\n",
            "GET\t/auth-callback?state=expected-state&code=ok\tHTTP/1.1\r\n\r\n",
            "GET /auth-callback?state=expected-state&code=ok\r\n\r\n",
            "GET http://localhost/auth-callback?state=expected-state&code=ok HTTP/1.1\r\n\r\n",
            "GET /auth-callback?state=expected-state&code=ok#fragment HTTP/1.1\r\n\r\n",
            "GET /auth-callback?state=expected-state&code=ok HTTP/1.1\n\n",
            "GET /auth-callback?state=expected-state&code=ok HTTP/1.1\r\nBad header: x\r\n\r\n",
            "GET /auth-callback?state=expected-state&code=ok HTTP/1.1\r\nHost\r\n\r\n",
            "GET /auth-callback?state=expected-state&code=ok HTTP/1.1\r\nHost: local\x00host\r\n\r\n",
        ] {
            assert_eq!(
                validate(request.as_bytes(), PATH, STATE),
                Reply::Invalid,
                "{request:?}"
            );
        }
    }

    #[test]
    fn requires_a_complete_bounded_head() {
        let request = head("/auth-callback?state=expected-state&code=ok");
        for end in 0..request.len() {
            assert_eq!(
                validate(&request.as_bytes()[..end], PATH, STATE),
                Reply::Invalid
            );
        }
        let prefix = "GET /auth-callback?state=expected-state&code=ok HTTP/1.1\r\nX-Pad: ";
        let request = format!(
            "{prefix}{}\r\n\r\n",
            "x".repeat(HEAD_LIMIT - prefix.len() - 4)
        );
        assert_eq!(
            validate(request.as_bytes(), PATH, STATE),
            Reply::Code("ok".into())
        );
        assert_eq!(
            validate(
                request.replace("X-Pad: ", "X-Pad: x").as_bytes(),
                PATH,
                STATE
            ),
            Reply::Invalid
        );
    }

    #[test]
    fn requires_unique_matching_state_and_nonempty_code() {
        for query in [
            "code=ok",
            "state=wrong&code=ok",
            "state=&code=ok",
            "state=expected-state",
            "state=expected-state&code=",
            "state=expected-state&code",
            "state=expected-state&state=expected-state&code=ok",
            "state=expected-state&%73tate=expected-state&code=ok",
            "state=expected-state&code=ok&code=again",
            "state=expected-state&code=ok&%63ode=again",
            "state=expected-state&code=ok&error=access_denied",
            "state=expected-state&code=%",
            "state=expected-state&code=%+1",
            "state=expected-state&code=%FF",
        ] {
            assert_eq!(query_reply(query, STATE), Reply::Invalid, "{query}");
        }
    }

    #[test]
    fn bounds_decoded_code_length() {
        let code = "%61".repeat(CODE_LIMIT);
        let request = head(&format!("/auth-callback?state={STATE}&code={code}"));
        assert_eq!(
            validate(request.as_bytes(), PATH, STATE),
            Reply::Code("a".repeat(CODE_LIMIT))
        );
        let request = head(&format!("/auth-callback?state={STATE}&code={code}%61"));
        assert_eq!(validate(request.as_bytes(), PATH, STATE), Reply::Invalid);
    }

    #[test]
    fn errors_require_unique_matching_state() {
        assert_eq!(
            query_reply("state=expected-state&error=access_denied", STATE),
            Reply::Refused
        );
        for query in [
            "error=access_denied",
            "state=wrong&error=access_denied",
            "state=expected-state&state=expected-state&error=access_denied",
            "state=expected-state&error=",
            "state=expected-state&error=access_denied&error=other",
            "state=expected-state&error=access_denied&code=",
        ] {
            assert_eq!(query_reply(query, STATE), Reply::Invalid, "{query}");
        }
    }

    #[test]
    fn unknown_paths_are_ignored_before_method_or_query_validation() {
        for request in [
            head("/favicon.ico"),
            head("/other?state=wrong&code=%"),
            head("/other?error=access_denied").replace("GET ", "POST "),
        ] {
            assert_eq!(validate(request.as_bytes(), PATH, STATE), Reply::NotFound);
        }
    }

    #[test]
    fn percent_decoding_is_strict() {
        assert_eq!(percent_decode("a+b%2Bc"), Some("a b+c".into()));
        for value in ["%", "%0", "%GG", "%+1", "%FF", "%C3%28"] {
            assert_eq!(percent_decode(value), None, "{value}");
        }
    }

    #[tokio::test]
    async fn invalid_callbacks_and_unknown_paths_do_not_end_login() {
        let callback = Callback::bind(&[0], PATH).await.unwrap();
        assert_eq!(
            callback.redirect,
            format!("http://localhost:{}{PATH}", callback.port)
        );
        let address = callback.listeners[0].local_addr().unwrap();
        let login = tokio::spawn(callback.code(STATE.into()));
        for (target, status) in [
            (
                "/auth-callback?state=wrong&code=secret-code",
                "400 Bad Request",
            ),
            ("/auth-callback?code=ok", "400 Bad Request"),
            (
                "/auth-callback?state=expected-state&code=",
                "400 Bad Request",
            ),
            (
                "/auth-callback?state=expected-state&state=expected-state&code=ok",
                "400 Bad Request",
            ),
            (
                "/auth-callback?state=expected-state&code=ok&code=again",
                "400 Bad Request",
            ),
            (
                "/auth-callback?state=wrong&error=%3Cscript%3Esecret-error%3C%2Fscript%3E",
                "400 Bad Request",
            ),
            ("/auth-callback?error=access_denied", "400 Bad Request"),
            ("/favicon.ico", "404 Not Found"),
            ("/other?state=expected-state&code=ok", "404 Not Found"),
        ] {
            let response = exchange(address, &head(target)).await;
            assert!(
                response.starts_with(&format!("HTTP/1.1 {status}\r\n")),
                "{response}"
            );
            assert!(!response.contains("secret"));
            assert!(!response.contains("<script>"));
            assert!(!login.is_finished());
        }
        let request = head("/auth-callback?state=expected-state&code=ok").replace("GET ", "POST ");
        assert!(
            exchange(address, &request)
                .await
                .starts_with("HTTP/1.1 400 Bad Request\r\n")
        );
        let request = head("/other?error=access_denied").replace("GET ", "POST ");
        assert!(
            exchange(address, &request)
                .await
                .starts_with("HTTP/1.1 404 Not Found\r\n")
        );
        let response = exchange(
            address,
            &head("/auth-callback?state=expected-state&code=secret-code"),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(!response.contains("secret-code"));
        assert_eq!(
            tokio::time::timeout(TEST_TIMEOUT, login)
                .await
                .unwrap()
                .unwrap(),
            Ok("secret-code".into())
        );
    }

    #[tokio::test]
    async fn incomplete_and_oversized_heads_do_not_end_login() {
        let callback = Callback::bind(&[0], PATH).await.unwrap();
        let address = callback.listeners[0].local_addr().unwrap();
        let login = tokio::spawn(callback.code(STATE.into()));
        let incomplete = "GET /auth-callback?state=expected-state&code=ok HTTP/1.1\r\n";
        assert!(
            exchange(address, incomplete)
                .await
                .starts_with("HTTP/1.1 400 Bad Request\r\n")
        );
        let oversized = format!("{incomplete}X-Pad: {}\r\n\r\n", "x".repeat(HEAD_LIMIT));
        assert!(
            exchange(address, &oversized)
                .await
                .starts_with("HTTP/1.1 400 Bad Request\r\n")
        );
        assert!(
            exchange(
                address,
                &head("/auth-callback?state=expected-state&code=ok")
            )
            .await
            .starts_with("HTTP/1.1 200 OK\r\n")
        );
        assert_eq!(
            tokio::time::timeout(TEST_TIMEOUT, login)
                .await
                .unwrap()
                .unwrap(),
            Ok("ok".into())
        );
    }

    #[tokio::test]
    async fn oauth_error_is_fixed_and_never_reflected() {
        let callback = Callback::bind(&[0], PATH).await.unwrap();
        let address = callback.listeners[0].local_addr().unwrap();
        let login = tokio::spawn(callback.code(STATE.into()));
        let request = head(
            "/auth-callback?state=expected-state&error=%3Cscript%3Esecret-credential%3C%2Fscript%3E&error_description=private-token",
        );
        let response = exchange(address, &request).await;
        assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
        for text in ["<script>", "%3C", "secret-credential", "private-token"] {
            assert!(!response.contains(text));
        }
        assert_eq!(
            tokio::time::timeout(TEST_TIMEOUT, login)
                .await
                .unwrap()
                .unwrap(),
            Err("the sign-in was refused".into())
        );
    }

    #[tokio::test]
    async fn completion_and_cancellation_close_all_preconnect_workers() {
        for outcome in ["code=ok", "error=access_denied", "cancel"] {
            let callback = Callback::bind(&[0], PATH).await.unwrap();
            let addresses: Vec<_> = callback
                .listeners
                .iter()
                .map(|listener| listener.local_addr().unwrap())
                .collect();
            let login = tokio::spawn(callback.code(STATE.into()));
            let mut preconnects = Vec::new();
            for &address in &addresses {
                let mut stream = TcpStream::connect(address).await.unwrap();
                stream.write_all(b"GET /auth-callback").await.unwrap();
                preconnects.push(stream);
                assert!(
                    exchange(address, &head("/favicon.ico"))
                        .await
                        .starts_with("HTTP/1.1 404 Not Found\r\n")
                );
            }
            if outcome == "cancel" {
                login.abort();
                assert!(login.await.unwrap_err().is_cancelled());
            } else {
                let response = exchange(
                    addresses[0],
                    &head(&format!("/auth-callback?state={STATE}&{outcome}")),
                )
                .await;
                let result = tokio::time::timeout(TEST_TIMEOUT, login)
                    .await
                    .unwrap()
                    .unwrap();
                if outcome == "code=ok" {
                    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
                    assert_eq!(result, Ok("ok".into()));
                } else {
                    assert!(response.starts_with("HTTP/1.1 400 Bad Request\r\n"));
                    assert_eq!(result, Err("the sign-in was refused".into()));
                }
            }
            for stream in preconnects {
                assert_closed(stream).await;
            }
            for address in addresses {
                assert!(
                    TcpStream::connect(address).await.is_err(),
                    "listener survived the callback"
                );
            }
        }
    }

    #[tokio::test]
    async fn existing_provider_callback_paths_still_work() {
        for path in [
            "/oauth-callback",
            "/auth/callback",
            "/callback",
            "/auth-callback",
        ] {
            let callback = Callback::bind(&[0], path).await.unwrap();
            let address = callback.listeners[0].local_addr().unwrap();
            assert_eq!(
                callback.redirect,
                format!("http://localhost:{}{path}", callback.port)
            );
            let login = tokio::spawn(callback.code(STATE.into()));
            let response = exchange(address, &head(&format!("{path}?state={STATE}&code=ok"))).await;
            assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
            assert_eq!(
                tokio::time::timeout(TEST_TIMEOUT, login)
                    .await
                    .unwrap()
                    .unwrap(),
                Ok("ok".into())
            );
        }
    }
}
