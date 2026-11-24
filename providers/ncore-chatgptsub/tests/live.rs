use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use ncore::core::ir::{Accumulator, Block, Message, Request, Role, StopReason};
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    CancelToken, Core, GenerateOptions, HttpRequest, HttpResponse, Transport, TransportError,
};
use ncore_chatgptsub::{ChatGptSub, Credentials, MemoryStore, oauth};
use ncore_tokio::{HyperTransport, Proxy};

struct LiveTransport(HyperTransport);

impl Transport for LiveTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        // This smoke test must not rotate credentials supplied from another client.
        if request.url == oauth::TOKEN_URL {
            return boxed(async {
                Err(TransportError::new(
                    "live test needs fresh credentials; refresh is disabled",
                ))
            });
        }
        self.0.send(request)
    }
}

#[tokio::test]
#[ignore = "requires explicitly supplied ChatGPT OAuth credentials and generates model tokens"]
async fn live_generate_and_monitor() {
    let path = std::env::var("NABU_CHATGPTSUB_CREDENTIALS")
        .expect("set NABU_CHATGPTSUB_CREDENTIALS to an isolated credentials file");
    let raw = std::fs::read(path).expect("could not read credentials");
    let credentials = serde_json::from_slice::<Credentials>(&raw)
        .or_else(|_| Credentials::from_codex_json(&raw))
        .expect("invalid credentials");
    let model = std::env::var("NABU_CHATGPTSUB_MODEL").ok();
    let proxy = std::env::var("NABU_CHATGPTSUB_PROXY")
        .ok()
        .map(|s| Proxy::parse(&s).expect("invalid test proxy"));
    let net = Arc::new(LiveTransport(HyperTransport::with_proxy(proxy)));
    let plugin = Arc::new(ChatGptSub::new(Arc::new(MemoryStore::new(credentials))));
    let endpoint = ChatGptSub::endpoint().with_plugin(plugin.clone());
    let core = Core::builder(net.clone()).build();
    tokio::time::timeout(Duration::from_secs(180), async {
        let usage = plugin.usage(&*net).await.expect("usage request failed");
        assert!(!usage.stale);
        let catalog = if let Ok(version) = std::env::var("NABU_CHATGPTSUB_CLIENT_VERSION") {
            Some(
                plugin
                    .models(&*net, &version)
                    .await
                    .expect("model catalog request failed"),
            )
        } else {
            None
        };
        let model = model
            .or_else(|| {
                catalog
                    .as_ref()
                    .and_then(|v| v["models"].as_array())
                    .and_then(|models| models.iter().find(|model| model["visibility"] == "list"))
                    .and_then(|model| model["slug"].as_str())
                    .map(str::to_owned)
            })
            .expect("set NABU_CHATGPTSUB_MODEL or provide a client version for model discovery");
        println!("Live model: {model}; usage and model discovery succeeded");
        for stream in [true, false] {
            let request = Request {
                model: model.clone(),
                system: vec![Block::text("Reply with exactly NABU_OK and nothing else.")],
                messages: vec![Message::new(
                    Role::User,
                    vec![Block::text("Run the smoke test.")],
                )],
                stream,
                ..Default::default()
            };
            let mut generation = core
                .generate(
                    &endpoint,
                    &request,
                    &GenerateOptions::default(),
                    &CancelToken::new(),
                )
                .await
                .expect("generation failed");
            let mut acc = Accumulator::new();
            while let Some(event) = generation.next().await {
                acc.push(&event.expect("stream failed"));
            }
            let response = generation.take_response().unwrap_or_else(|| acc.finish());
            assert_eq!(response.stop, Some(StopReason::EndTurn));
            assert!(response.content.iter().any(
                |block| matches!(block, Block::Text { text, .. } if text.trim() == "NABU_OK")
            ));
            println!(
                "Generation stream={stream}: NABU_OK; usage={:?}",
                response.usage
            );
        }
    })
    .await
    .expect("live test timed out");
}
