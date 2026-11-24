//! `skill`: loads the instructions of a skill.

use ncore::core::ir::ToolDef;
use serde_json::{Value, json};

use super::{def as tool_def, str_arg};
use crate::skills::{self, Skill};

pub fn def(skills: &[Skill]) -> ToolDef {
    let list: Vec<String> = skills.iter().map(Skill::listed).collect();
    tool_def(
        "skill",
        &format!(
            "Loads a skill: instructions for a kind of task, kept by the person in skill folders. \
When the task matches a skill below, load it before doing the task and follow what it says. \
A skill the person marks in a message shows there as <skill name=\"…\"/>, and its text comes with the message: \
follow it without loading it again.\nSkills:\n{}",
            list.join("\n")
        ),
        json!({
            "type": "object",
            "properties": { "name": { "type": "string", "description": "The skill's name from the list." } },
            "required": ["name"]
        }),
    )
}

pub fn run(skills: &[Skill], args: &Value) -> Result<String, String> {
    let name = str_arg(args, "name")?;
    let Some(skill) = skills.iter().find(|s| s.name == name) else {
        let names: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        return Err(format!("no skill `{name}`; skills here: {}", names.join(", ")));
    };
    Ok(skills::content(name, &skill.dir().display().to_string(), &skill.body()?))
}
