use nabu_protocol::{Event, Part, Question, QuestionRequest, QuestionTool};
use ncore::CancelToken;
use serde_json::json;
use tokio::sync::oneshot;

use super::{Sessions, id, missing};

/// Answers for each question, or `None` when the user dismissed them.
type Answers = Option<Vec<Vec<String>>>;

/// Questions waiting for the user, and where the answer goes.
pub(super) struct Waiting {
    directory: String,
    request: QuestionRequest,
    answer: oneshot::Sender<Answers>,
}

impl Sessions {
    /// Puts the questions of an `ask` call to the user and waits for the answers.
    pub async fn ask(
        &self,
        session: &str,
        call_id: &str,
        questions: Vec<Question>,
        cancel: &CancelToken,
    ) -> Result<Answers, String> {
        let (sender, answer) = oneshot::channel();
        let (directory, request) = {
            let mut state = self.state.lock().unwrap();
            let doc = state.docs.get(session).ok_or_else(|| missing(session))?;
            let message_id = doc
                .messages
                .iter()
                .rev()
                .flat_map(|m| &m.parts)
                .find_map(|p| match p {
                    Part::Tool(t) if t.call_id == call_id => Some(t.message_id.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            let tool = QuestionTool {
                message_id,
                call_id: call_id.into(),
            };
            let request = QuestionRequest {
                id: id::ascending("que"),
                session_id: session.into(),
                questions,
                tool,
            };
            let directory = doc.session.directory.clone();
            state.questions.insert(
                request.id.clone(),
                Waiting {
                    directory: directory.clone(),
                    request: request.clone(),
                    answer: sender,
                },
            );
            (directory, request)
        };
        let _closes = Closing {
            sessions: self,
            id: request.id.clone(),
        };
        self.emit(Event::QuestionAsked { directory, request });
        let answers = tokio::select! {
            answers = answer => answers.unwrap_or(None),
            _ = cancel.cancelled() => return Err("cancelled".into()),
        };
        if let Some(answers) = &answers {
            self.link(session, call_id, json!({ "answers": answers }));
        }
        Ok(answers)
    }

    pub fn questions(&self, directory: &str) -> Vec<QuestionRequest> {
        let state = self.state.lock().unwrap();
        let mut list: Vec<QuestionRequest> = state
            .questions
            .values()
            .filter(|w| w.directory == directory)
            .map(|w| w.request.clone())
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    /// Hands the user's answers to the waiting call.
    pub fn reply_question(&self, request_id: &str, answers: Answers) -> Result<(), String> {
        let waiting = self
            .close_question(request_id)
            .ok_or("these questions were already answered or are gone")?;
        let _ = waiting.answer.send(answers);
        Ok(())
    }

    fn close_question(&self, request_id: &str) -> Option<Waiting> {
        let waiting = self.state.lock().unwrap().questions.remove(request_id)?;
        let (directory, session_id) = (
            waiting.directory.clone(),
            waiting.request.session_id.clone(),
        );
        self.emit(Event::QuestionClosed {
            directory,
            session_id,
            request_id: request_id.into(),
        });
        Some(waiting)
    }
}

/// Takes the questions down when the call ends without an answer, like on cancel.
struct Closing<'a> {
    sessions: &'a Sessions,
    id: String,
}

impl Drop for Closing<'_> {
    fn drop(&mut self) {
        self.sessions.close_question(&self.id);
    }
}
