//! Skill and command marks the person puts in a message.

use std::sync::Arc;

use nabu_protocol::{CommandInfo, Prompt, PromptPart, SkillPart, SkillSource};

use super::{Sessions, missing};
use crate::skills::{self, MAX_MARKS, Skill};

/// Commands the server runs, with what each does.
const COMMANDS: [(&str, &str); 1] = [("compact", "Compacts the session's history into a handoff")];

pub fn commands() -> Vec<CommandInfo> {
    COMMANDS.iter().map(|(name, description)| CommandInfo { name: (*name).into(), description: (*description).into() }).collect()
}

/// A marked skill's text as it was when the message was sent.
pub(super) struct Marked {
    name: String,
    path: String,
    body: String,
}

impl Marked {
    pub(super) fn name(&self) -> &str {
        &self.name
    }

    pub(super) fn part(&self, id: String, session_id: &str, message_id: &str, at: bool) -> SkillPart {
        SkillPart {
            id,
            session_id: session_id.into(),
            message_id: message_id.into(),
            name: self.name.clone(),
            at,
            path: self.path.clone(),
            body: self.body.clone(),
        }
    }
}

impl Sessions {
    pub fn skill_sources(&self) -> Vec<SkillSource> {
        self.state.lock().unwrap().skill_sources.clone()
    }

    pub fn set_skill_sources(&self, sources: Vec<SkillSource>) -> Result<(), String> {
        let repeated = sources.iter().enumerate().any(|(i, s)| sources[..i].iter().any(|o| (o.harness, o.scope) == (s.harness, s.scope)));
        if repeated {
            return Err("a skill source is listed twice".into());
        }
        self.state.lock().unwrap().skill_sources = sources;
        Ok(())
    }

    /// Skills a session in `directory` can use.
    pub fn skills(&self, directory: &str) -> Vec<Skill> {
        skills::find(&self.skill_sources(), std::path::Path::new(directory))
    }

    /// Sends the message; one with a command runs it first, the rest of the message waits for it in the queue.
    pub async fn send(self: &Arc<Self>, prompt: Prompt) -> Result<(), String> {
        let Some(name) = command_of(&prompt)? else {
            return self.prompt(prompt);
        };
        self.marked(&prompt)?;
        let rest = without_command(prompt.clone());
        let rest = (!rest.parts.is_empty()).then_some(rest);
        match name.as_str() {
            "compact" => self.compact(&prompt.session_id, prompt.model, "", rest).await,
            _ => Err(format!("unknown command /{name}")),
        }
    }

    /// The skills the message marks, each once, read now so the history keeps what the model saw.
    pub(super) fn marked(&self, prompt: &Prompt) -> Result<Vec<Marked>, String> {
        let names: Vec<&str> = prompt
            .parts
            .iter()
            .filter_map(|p| match p {
                PromptPart::Skill { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        if names.is_empty() {
            return Ok(Vec::new());
        }
        if names.len() > MAX_MARKS {
            return Err(format!("a message can mark at most {MAX_MARKS} skills"));
        }
        let directory = {
            let state = self.state.lock().unwrap();
            let doc = state.docs.get(&prompt.session_id).ok_or_else(|| missing(&prompt.session_id))?;
            doc.session.directory.clone()
        };
        let skills = self.skills(&directory);
        let mut marked: Vec<Marked> = Vec::new();
        for name in names {
            if marked.iter().any(|m| m.name == name) {
                return Err(format!("skill {name} is marked twice"));
            }
            let skill = skills.iter().find(|s| s.name == name).ok_or_else(|| format!("unknown skill {name}"))?;
            let (path, body) = (skill.dir().display().to_string(), skill.body()?);
            marked.push(Marked { name: name.into(), path, body });
        }
        Ok(marked)
    }
}

/// The command a message marks; a message marks one at most.
fn command_of(prompt: &Prompt) -> Result<Option<String>, String> {
    let commands: Vec<&str> = prompt
        .parts
        .iter()
        .filter_map(|p| match p {
            PromptPart::Command { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    match commands[..] {
        [] => Ok(None),
        [name] => Ok(Some(name.into())),
        _ => Err("a message marks one command at most".into()),
    }
}

/// The message without its command: the texts around it join into one, with one space where the command stood.
fn without_command(mut prompt: Prompt) -> Prompt {
    let mut parts: Vec<PromptPart> = Vec::new();
    for part in prompt.parts {
        if let PromptPart::Command { .. } = part {
            continue;
        }
        if let (PromptPart::Text { text, .. }, Some(PromptPart::Text { text: before, .. })) = (&part, parts.last_mut()) {
            before.push_str(if before.ends_with(char::is_whitespace) { text.trim_start() } else { text });
            continue;
        }
        parts.push(part);
    }
    if let Some(PromptPart::Text { text, .. }) = parts.first_mut() {
        *text = text.trim_start().into();
    }
    if let Some(PromptPart::Text { text, .. }) = parts.last_mut() {
        *text = text.trim_end().into();
    }
    parts.retain(|part| !matches!(part, PromptPart::Text { text, .. } if text.is_empty()));
    prompt.parts = parts;
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;
    use nabu_protocol::ModelRef;

    fn prompt(parts: Vec<PromptPart>) -> Prompt {
        let model = ModelRef { provider_id: "p".into(), model_id: "m".into(), variant: None, fast: false };
        Prompt { session_id: "s".into(), message_id: "m".into(), agent: "build".into(), model, parts }
    }

    #[test]
    fn a_message_marks_one_command_and_may_say_more() {
        let command = || PromptPart::Command { id: "c".into(), name: "compact".into() };
        let text = |text: &str| PromptPart::Text { id: "t".into(), text: text.into() };
        assert_eq!(command_of(&prompt(vec![text("/compact please")])).unwrap(), None);
        let alone = prompt(vec![command(), text(" ")]);
        assert_eq!(command_of(&alone).unwrap().as_deref(), Some("compact"));
        assert!(without_command(alone).parts.is_empty());
        let with_text = prompt(vec![text("now"), command()]);
        assert_eq!(command_of(&with_text).unwrap().as_deref(), Some("compact"));
        assert_eq!(texts(&without_command(with_text)), ["now"]);
        assert!(command_of(&prompt(vec![command(), command()])).is_err());
    }

    #[test]
    fn the_rest_of_a_message_reads_as_if_the_command_was_never_typed() {
        let command = || PromptPart::Command { id: "c".into(), name: "compact".into() };
        let text = |text: &str| PromptPart::Text { id: "t".into(), text: text.into() };
        let skill = || PromptPart::Skill { id: "s".into(), name: "review".into(), at: false };
        assert_eq!(texts(&without_command(prompt(vec![command(), text(" then look"), skill()]))), ["then look", "<review>"]);
        assert_eq!(texts(&without_command(prompt(vec![text("look "), command(), text(" now")]))), ["look now"]);
        assert_eq!(texts(&without_command(prompt(vec![skill(), text(" "), command(), text(" and")]))), ["<review>", " and"]);
    }

    fn texts(prompt: &Prompt) -> Vec<String> {
        prompt.parts.iter().map(|part| match part {
            PromptPart::Text { text, .. } => text.clone(),
            PromptPart::Skill { name, .. } => format!("<{name}>"),
            _ => "?".into(),
        }).collect()
    }
}
