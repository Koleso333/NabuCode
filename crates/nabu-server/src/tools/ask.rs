use nabu_protocol::{Question, QuestionOption};
use ncore::CancelToken;
use ncore::core::ir::ToolDef;
use serde_json::{Value, json};

use crate::sessions::Sessions;

const QUESTIONS_MAX: usize = 10;
const OPTIONS: std::ops::RangeInclusive<usize> = 2..=6;
const HEADER_CHARS: usize = 30;
/// The UI tells a dismissed call by this text.
const DISMISSED: &str =
    "the user dismissed this question and stopped the turn; wait for their next message";

pub fn def() -> ToolDef {
    super::def(
        "ask",
        "Shows the user questions with options in a panel and waits for the answers. Use it to get the user's choice \
or a missing fact during the work. Several questions go in one call, one tab each. The user picks an option (or several, with `multiple`) \
or types an answer of their own, so an \"Other\" option is not needed. A label ending with \"(recommended)\" marks the option you recommend. \
If the user dismisses the questions, the turn ends.",
        json!({
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "1 to 10 questions.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string", "description": "The full question, ending with a question mark." },
                            "header": { "type": "string", "description": "A label for the question's tab, up to 30 characters." },
                            "options": {
                                "type": "array",
                                "description": "2 to 6 distinct choices.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string", "description": "The choice in 1-5 words." },
                                        "description": { "type": "string", "description": "What the choice means or leads to." }
                                    },
                                    "required": ["label", "description"]
                                }
                            },
                            "multiple": { "type": "boolean", "description": "The user may pick several options. Default false." }
                        },
                        "required": ["question", "header", "options"]
                    }
                }
            },
            "required": ["questions"]
        }),
    )
}

pub async fn run(
    sessions: &Sessions,
    session: &str,
    call_id: &str,
    args: &Value,
    cancel: &CancelToken,
) -> Result<String, String> {
    let questions = parse(args)?;
    match sessions
        .ask(session, call_id, questions.clone(), cancel)
        .await?
    {
        Some(answers) => Ok(report(&questions, &answers)),
        None => {
            sessions.abort(session);
            Err(DISMISSED.into())
        }
    }
}

fn parse(args: &Value) -> Result<Vec<Question>, String> {
    let list = args
        .get("questions")
        .and_then(Value::as_array)
        .ok_or("missing `questions`: an array of questions")?;
    if list.is_empty() || list.len() > QUESTIONS_MAX {
        return Err(format!(
            "give 1 to {QUESTIONS_MAX} questions, not {}",
            list.len()
        ));
    }
    list.iter()
        .enumerate()
        .map(|(i, q)| question(q).map_err(|e| format!("question {}: {e}", i + 1)))
        .collect()
}

fn question(value: &Value) -> Result<Question, String> {
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let question = text("question").ok_or("missing `question`")?.to_owned();
    let header = text("header").ok_or("missing `header`")?;
    if header.chars().count() > HEADER_CHARS {
        return Err(format!("`header` is longer than {HEADER_CHARS} characters"));
    }
    let options = value
        .get("options")
        .and_then(Value::as_array)
        .ok_or("missing `options`")?;
    if !OPTIONS.contains(&options.len()) {
        return Err(format!(
            "give {} to {} options, not {}",
            OPTIONS.start(),
            OPTIONS.end(),
            options.len()
        ));
    }
    let mut parsed: Vec<QuestionOption> = Vec::new();
    for option in options {
        let label = option
            .get("label")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or("an option has no `label`")?;
        if parsed.iter().any(|o| o.label == label) {
            return Err(format!("two options are labeled `{label}`"));
        }
        let description = option
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        parsed.push(QuestionOption {
            label: label.into(),
            description,
        });
    }
    let multiple = value
        .get("multiple")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Question {
        question,
        header: header.into(),
        options: parsed,
        multiple,
    })
}

/// Each question with what the user picked; typed answers are marked as such.
fn report(questions: &[Question], answers: &[Vec<String>]) -> String {
    let lines: Vec<String> = questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let picked: Vec<String> = answers
                .get(i)
                .into_iter()
                .flatten()
                .map(|a| match q.options.iter().any(|o| o.label == *a) {
                    true => a.clone(),
                    false => format!("\"{a}\" (typed by the user)"),
                })
                .collect();
            let answer = if picked.is_empty() {
                "(no answer)".to_owned()
            } else {
                picked.join(", ")
            };
            format!("{}\n→ {answer}", q.question)
        })
        .collect();
    format!("The user answered:\n{}", lines.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_are_checked() {
        let one = |options: Value| json!({ "questions": [{ "question": "Which?", "header": "Pick", "options": options }] });
        assert!(
            parse(&one(json!([{ "label": "a" }])))
                .unwrap_err()
                .contains("2 to 6 options")
        );
        assert!(
            parse(&one(json!([{ "label": "a" }, { "label": "a" }])))
                .unwrap_err()
                .contains("two options are labeled `a`")
        );
        assert_eq!(
            parse(&one(
                json!([{ "label": "a" }, { "label": "b", "description": "B" }])
            ))
            .unwrap()[0]
                .options[1]
                .description,
            "B"
        );
    }

    #[test]
    fn typed_and_missing_answers_are_marked() {
        let args = json!({ "questions": [
            { "question": "Which?", "header": "Pick", "options": [{ "label": "a" }, { "label": "b" }], "multiple": true },
            { "question": "Name?", "header": "Name", "options": [{ "label": "x" }, { "label": "y" }] },
            { "question": "Skip?", "header": "Skip", "options": [{ "label": "x" }, { "label": "y" }] }
        ]});
        let questions = parse(&args).unwrap();
        let answers = vec![vec!["a".into(), "b".into()], vec!["mine".into()], vec![]];
        assert_eq!(
            report(&questions, &answers),
            "The user answered:\nWhich?\n→ a, b\n\nName?\n→ \"mine\" (typed by the user)\n\nSkip?\n→ (no answer)"
        );
    }
}
