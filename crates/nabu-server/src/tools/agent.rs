use nabu_protocol::Provider;
use ncore::core::ir::ToolDef;
use serde_json::{Value, json};

use super::{bool_arg, opt_str, str_arg};

/// What the agent asked a subagent to do.
pub struct Ask {
    /// A subagent to continue, like `sub1`.
    pub id: Option<String>,
    pub prompt: String,
    pub description: String,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub tools: Option<Vec<String>>,
    pub background: bool,
}

impl Ask {
    pub fn parse(args: &Value, known: &[&str]) -> Result<Self, String> {
        let id = opt_str(args, "id").map(str::to_owned);
        let description = opt_str(args, "description").map(str::to_owned);
        if id.is_none() && description.is_none() {
            return Err(
                "missing string argument `description`: a few words saying what the subagent does"
                    .into(),
            );
        }
        Ok(Self {
            prompt: str_arg(args, "prompt")?.to_owned(),
            description: description.unwrap_or_default(),
            model: opt_str(args, "model").map(str::to_owned),
            effort: opt_str(args, "effort").map(str::to_owned),
            tools: tools(args, known)?,
            background: bool_arg(args, "background"),
            id,
        })
    }
}

/// The allowed tools; `None` when the agent did not limit them.
fn tools(args: &Value, known: &[&str]) -> Result<Option<Vec<String>>, String> {
    let Some(list) = args.get("tools").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let names: Vec<String> = list
        .as_array()
        .ok_or("`tools` must be a list of tool names")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(str::to_owned)
                .ok_or("`tools` must be a list of tool names")
        })
        .collect::<Result<_, _>>()?;
    if let Some(bad) = names.iter().find(|n| !known.contains(&n.as_str())) {
        return Err(format!(
            "`tools` names `{bad}`, which a subagent cannot have; it can have {}",
            known.join(", ")
        ));
    }
    Ok(Some(names))
}

pub fn def(providers: &[Provider], current: &str, tools: &[&str]) -> ToolDef {
    let models: Vec<String> = providers
        .iter()
        .flat_map(|p| {
            p.models.iter().filter(|m| !m.older).map(move |m| {
                let efforts = if m.efforts.is_empty() {
                    String::new()
                } else {
                    format!(", effort: {}", m.efforts.join("/"))
                };
                format!("- {}/{} ({}{efforts})", p.id, m.id, m.name)
            })
        })
        .collect();
    let description = format!(
        "Starts a subagent: another agent with its own fresh context that does one task on its own and reports back. \
Use it for work that is independent of what you are doing, needs many steps or reads much you will not need later \
(broad searches, research, a separate change), or for work in parallel: several `agent` calls in one step run at the same time. \
The subagent sees nothing of this conversation, only `prompt`: give it everything it needs — the goal, paths, what you already \
know, constraints, and what its report must contain. It works in the same directory with the same tools \
(except `agent`), unless `tools` limits them. When it finishes, its final message is your result; the person does not see it, \
so tell them what matters. \
By default you wait for the report. With `background: true` the call returns at once with an id like \"sub1\"; the report \
comes as a notice with your next tool result or message, and if you are idle by then, a new turn starts with it. \
`task` with that id shows its state, `wait`s for the report or `stop`s it. \
To continue a finished subagent with what it already knows, call `agent` with its `id` and a new `prompt`.\n\
Models (provider/model; default {current}, the one you run on):\n{}\n\
Tools a subagent can have: {}.",
        models.join("\n"),
        tools.join(", ")
    );
    super::def(
        "agent",
        &description,
        json!({
            "type": "object",
            "properties": {
                "description": { "type": "string", "description": "3 to 6 words on what the subagent does; shown to the person." },
                "prompt": { "type": "string", "description": "The whole task for the subagent, self-contained." },
                "model": { "type": "string", "description": "provider/model from the list. Omit to use your own model." },
                "effort": { "type": "string", "description": "Reasoning effort, one the model lists. Omit for the model's default." },
                "tools": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "The only tools the subagent may use, e.g. [\"read\", \"grep\", \"glob\", \"list\"] for one that must not change anything. Omit for all."
                },
                "background": { "type": "boolean", "description": "Return at once; the report comes later as a notice." },
                "id": { "type": "string", "description": "A finished subagent to continue, like \"sub1\"; it keeps its context, model and tools unless you give new ones." }
            },
            "required": ["prompt"]
        }),
    )
}
