
use std::sync::Arc;

use ncore::rt::{BoxFuture, boxed};
use ncore::{Auth, Endpoint, Format, Hook, HttpRequest, Interests, RequestCtx};

#[derive(Debug, Default, Clone, Copy)]
pub struct AgentRouterBP;

impl AgentRouterBP {
    pub fn endpoint(api_key: impl Into<String>) -> Endpoint {
        Endpoint::new(Format::OpenAiResponses, "https://agentrouter.org")
            .with_auth(Auth::Bearer(api_key.into()))
            .with_hook(Arc::new(Self))
    }
}

impl Hook for AgentRouterBP {
    fn name(&self) -> &str {
        "agentrouter-bp"
    }

    fn interests(&self) -> Interests {
        Interests {
            request: true,
            ..Interests::NONE
        }
    }

    fn on_request<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        req: &'a mut HttpRequest,
    ) -> BoxFuture<'a, Result<(), String>> {
        req.headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("originator"));
        req.headers
            .push(("originator".into(), "codex_cli_rs".into()));
        if !req
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("user-agent"))
        {
            req.headers.push((
                "user-agent".into(),
                concat!("nabucore-agentrouter-bp/", env!("CARGO_PKG_VERSION")).into(),
            ));
        }
        boxed(async { Ok(()) })
    }
}
