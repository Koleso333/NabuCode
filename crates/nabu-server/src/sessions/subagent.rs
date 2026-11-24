use std::sync::Arc;
use std::time::Instant;

use nabu_protocol::{Message, ModelRef, Part, Provider};
use ncore::CancelToken;
use serde_json::json;

use super::Sessions;
use crate::tools::{Ask, Jobs, Signals};

/// Starts subagents for the agent of one session; each works in a child session of it.
pub struct Spawner {
    pub sessions: Arc<Sessions>,
    pub parent: String,
    /// The model the agent runs on, the subagent's default.
    pub model: ModelRef,
}

impl Spawner {
    pub fn providers(&self) -> Vec<Provider> {
        self.sessions.models.providers()
    }

    /// `provider/model` of the agent's own model.
    pub fn current(&self) -> String {
        format!("{}/{}", self.model.provider_id, self.model.model_id)
    }

    pub async fn run(
        &self,
        call_id: &str,
        ask: Ask,
        jobs: &Arc<Jobs>,
        signals: &Signals,
    ) -> Result<String, String> {
        let (child, description, last) = match &ask.id {
            Some(id) => {
                let (child, description) = jobs.sub_session(id)?;
                if ask.tools.is_some() {
                    self.sessions.set_tools(&child, ask.tools.clone());
                }
                let last = self.sessions.last_model(&child);
                (child, description, last)
            }
            None => {
                let child = self.sessions.create_child(
                    &self.parent,
                    &ask.description,
                    ask.tools.clone(),
                )?;
                (child.id, ask.description.clone(), None)
            }
        };
        let model = self.model(&ask, last)?;
        let name = self.sessions.models.name(&model);
        let run_cancel = CancelToken::new();
        let id = jobs.sub_start(
            ask.id.as_deref(),
            &child,
            &description,
            ask.background,
            &run_cancel,
        );
        self.sessions.link(
            &self.parent,
            call_id,
            json!({ "sessionId": child, "background": ask.background, "id": id }),
        );

        let started = Instant::now();
        let child_id = child.clone();
        let turn = {
            let (sessions, jobs, id) = (self.sessions.clone(), jobs.clone(), id.clone());
            let cancel = run_cancel.clone();
            async move {
                let report = sessions
                    .subagent_turn(&child, model, ask.prompt, cancel)
                    .await;
                jobs.sub_end(&id, &report);
                report
            }
        };
        if ask.background {
            tokio::spawn(turn);
            return Ok(format!(
                "{id} started in the background on {name}. Its report comes as a notice with your next tool result or message; \
`task` with id \"{id}\" shows its state, `wait`s for it or `stop`s it."
            ));
        }
        // Stopping the agent stops the subagent it waits for; a message from the person leaves it working on its own.
        let mut turn = Box::pin(turn);
        let report = tokio::select! {
            report = &mut turn => report,
            _ = signals.cancel.cancelled() => {
                run_cancel.cancel();
                turn.await
            }
            _ = signals.wrap_up.cancelled() => {
                jobs.sub_detach(&id);
                tokio::spawn(turn);
                self.sessions.link(&self.parent, call_id, json!({ "sessionId": child_id, "background": true, "id": id }));
                return Ok(format!(
                    "the person sent you a message, so {id} goes on in the background on {name}. Its report comes as a notice \
with your next tool result or message; `task` with id \"{id}\" shows its state, `wait`s for it or `stop`s it."
                ));
            }
        };
        Ok(format!(
            "{id} finished on {name} in {:.0} s. Its report:\n{report}\n\n(`agent` with id \"{id}\" and a new `prompt` continues it; it remembers all it did.)",
            started.elapsed().as_secs_f64()
        ))
    }

    /// The model asked for, else the one the subagent used last, else the agent's own.
    fn model(&self, ask: &Ask, last: Option<ModelRef>) -> Result<ModelRef, String> {
        let mut model = match &ask.model {
            Some(wanted) => self.find(wanted)?,
            None => last.unwrap_or_else(|| self.model.clone()),
        };
        if ask.effort.is_some() {
            model.variant = ask.effort.clone();
        }
        Ok(model)
    }

    /// `provider/model`, a bare model id or a model name.
    fn find(&self, wanted: &str) -> Result<ModelRef, String> {
        let providers = self.providers();
        let (provider, id) = wanted
            .split_once('/')
            .map_or((None, wanted), |(p, m)| (Some(p), m));
        let found = providers
            .iter()
            .filter(|p| provider.is_none_or(|w| w == p.id))
            .find_map(|p| {
                p.models
                    .iter()
                    .find(|m| m.id == id || m.name.eq_ignore_ascii_case(id))
                    .map(|m| ModelRef {
                        provider_id: p.id.clone(),
                        model_id: m.id.clone(),
                        variant: None,
                        fast: false,
                    })
            });
        found.ok_or_else(|| {
            let all: Vec<String> = providers
                .iter()
                .flat_map(|p| p.models.iter().map(move |m| format!("{}/{}", p.id, m.id)))
                .collect();
            format!("no model `{wanted}`; available: {}", all.join(", "))
        })
    }
}

/// The subagent's final words in the turn after `user`, and the error it stopped with, if any.
pub fn report(messages: &[nabu_protocol::MessageWithParts], user: &str) -> String {
    let turn: Vec<_> = messages
        .iter()
        .skip_while(|m| m.info.id() != user)
        .skip(1)
        .collect();
    let text = turn.iter().rev().find_map(|m| {
        let text: Vec<&str> = m
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::Text(t) if !t.synthetic && !t.text.trim().is_empty() => Some(t.text.trim()),
                _ => None,
            })
            .collect();
        (!text.is_empty()).then(|| text.join("\n\n"))
    });
    let error = turn.last().and_then(|m| match &m.info {
        Message::Assistant(a) => a.error.as_ref().map(|e| e.data.message.clone()),
        Message::User(_) => None,
    });
    let text = text.unwrap_or_else(|| "(the subagent wrote no final message)".into());
    match error {
        Some(error) => format!("{text}\n\n(The subagent stopped before finishing: {error})"),
        None => text,
    }
}
