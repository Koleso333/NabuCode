use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use nabu_protocol::{FilePart, MessageWithParts, Part, Session, ToolState};
use serde::{Deserialize, Serialize};

/// Attachments live next to the session file; the session keeps only this reference, in memory too.
const ATTACHMENT: &str = "attachment:";
/// A file on this computer bigger than this is not copied: the session keeps the path of the original.
const COPY_LIMIT: u64 = 100 * 1024 * 1024;

/// One session with its history, as kept in `sessions/<id>.json`; attachments in `sessions/<id>/`.
#[derive(Clone, Serialize, Deserialize)]
pub struct Doc {
    pub session: Session,
    pub messages: Vec<MessageWithParts>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact: Option<super::compact::Checkpoint>,
    /// The only tools a subagent may use; `None` means all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
}

pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn load_all(&self) -> Vec<Doc> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .filter_map(|p| match self.load(&p) {
                Ok(doc) => Some(doc),
                Err(e) => {
                    eprintln!("[sessions] skipping {}: {e}", p.display());
                    None
                }
            })
            .collect()
    }

    pub fn load_one(&self, id: &str) -> Result<Doc, String> {
        self.load(&self.path(id))
    }

    fn load(&self, path: &Path) -> Result<Doc, String> {
        let raw = std::fs::read(path).map_err(|e| e.to_string())?;
        serde_json::from_slice(&raw).map_err(|e| e.to_string())
    }

    pub fn save(&self, doc: &mut Doc) {
        if let Err(e) = self.save_checked(doc) {
            eprintln!("[sessions] could not save {}: {e}", doc.session.id);
        }
    }

    pub fn save_checked(&self, doc: &mut Doc) -> std::io::Result<()> {
        let path = self.path(&doc.session.id);
        std::fs::create_dir_all(&self.dir)?;
        self.keep_files(&doc.session.id, files(doc.messages.iter_mut().flat_map(|m| m.parts.iter_mut())))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(doc).expect("serializable"))?;
        std::fs::rename(tmp, &path)
    }

    /// Moves the files of new parts into the session's folder and leaves references to them.
    pub fn keep(&self, session: &str, parts: &mut [Part]) {
        if let Err(e) = self.keep_files(session, files(parts.iter_mut())) {
            eprintln!("[sessions] could not keep the files of {session}: {e}");
        }
    }

    fn keep_files<'a>(&self, session: &str, files: impl Iterator<Item = &'a mut FilePart>) -> std::io::Result<()> {
        let dir = self.attachments(session);
        for file in files {
            if let Some(name) = keep(&dir, file)? {
                file.url = format!("{ATTACHMENT}{name}");
            }
        }
        Ok(())
    }

    pub fn remove(&self, id: &str) {
        let removed =
            std::fs::remove_file(self.path(id)).and_then(|_| {
                match std::fs::remove_dir_all(self.attachments(id)) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    other => other,
                }
            });
        if let Err(e) = removed
            && e.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("[sessions] could not remove {id}: {e}");
        }
    }

    /// Points the kept files to the `/file` link, for the clients.
    pub fn link_files(&self, session: &str, messages: &mut [MessageWithParts]) {
        self.link(session, messages.iter_mut().flat_map(|m| m.parts.iter_mut()));
    }

    pub fn link<'a>(&self, session: &str, parts: impl Iterator<Item = &'a mut Part>) {
        for file in files(parts) {
            if let Some(name) = file.url.strip_prefix(ATTACHMENT) {
                file.url = format!("/file?session={}&name={name}", safe(session));
            }
        }
    }

    /// The kept attachment a `/file` link names; `None` for a name that could leave the folder.
    pub fn upload(&self, file: &FilePart) -> std::io::Result<(PathBuf, String)> {
        let dir = self.attachments(&file.session_id);
        std::fs::create_dir_all(&dir)?;
        let name = kept_name(file);
        Ok((dir.join(&name), format!("{ATTACHMENT}{name}")))
    }

    pub fn attachment(&self, session: &str, name: &str) -> Option<PathBuf> {
        let plain = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.');
        (plain && !name.starts_with('.')).then(|| self.attachments(session).join(name))
    }

    /// The session's file and attachments, by their names inside the sessions folder.
    pub fn files_of(&self, id: &str) -> Vec<(String, PathBuf)> {
        let mut files = vec![(format!("{}.json", safe(id)), self.path(id))];
        if let Ok(items) = std::fs::read_dir(self.attachments(id)) {
            for item in items.flatten().filter(|item| item.path().is_file()) {
                files.push((format!("{}/{}", safe(id), item.file_name().to_string_lossy()), item.path()));
            }
        }
        files
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{}.json", safe(id)))
    }

    pub(super) fn attachments(&self, id: &str) -> PathBuf {
        self.dir.join(safe(id))
    }
}

fn files<'a>(parts: impl Iterator<Item = &'a mut Part>) -> impl Iterator<Item = &'a mut FilePart> {
    parts
        .flat_map(|p| match p {
            Part::File(file) => std::slice::from_mut(file),
            Part::Tool(tool) => match &mut tool.state {
                ToolState::Completed { attachments, .. } => attachments.as_mut_slice(),
                _ => &mut [],
            },
            _ => &mut [],
        })
}

/// Writes the file of a `data:` URL, or copies a local file up to the limit, once; returns its name.
/// `None` leaves the URL as it is: a reference already, a folder, or a file kept by its path.
fn keep(dir: &Path, file: &FilePart) -> std::io::Result<Option<String>> {
    let name = kept_name(file);
    let path = dir.join(&name);
    if let Some(data) = base64_in(&file.url) {
        if !path.exists() {
            let bytes = STANDARD.decode(data).map_err(std::io::Error::other)?;
            std::fs::create_dir_all(dir)?;
            std::fs::write(path, bytes)?;
        }
        return Ok(Some(name));
    }
    let Some(original) = local_path(&file.url).filter(|original| original.is_file()) else {
        return Ok(None);
    };
    if path.exists() {
        return Ok(Some(name));
    }
    if std::fs::metadata(&original)?.len() > COPY_LIMIT {
        return Ok(None);
    }
    let copied = std::fs::create_dir_all(dir).and_then(|_| std::fs::copy(&original, &path));
    // A file that can't be copied still reaches the model by its path
    Ok(copied.inspect_err(|e| eprintln!("[sessions] could not copy {}: {e}", original.display())).ok().map(|_| name))
}

fn base64_in(url: &str) -> Option<&str> {
    url.strip_prefix("data:")?.split_once(";base64,").map(|(_, data)| data)
}

/// The file in base64, as providers take it: read from the session's folder only now.
pub(super) fn base64_of(dir: &Path, file: &FilePart) -> Option<String> {
    match file.url.strip_prefix(ATTACHMENT) {
        Some(name) => std::fs::read(dir.join(name)).ok().map(|bytes| STANDARD.encode(bytes)),
        None => base64_in(&file.url).map(str::to_owned),
    }
}

/// The place of a file or folder on this computer that a part refers to by a `file:` URL.
pub(super) fn local_path(url: &str) -> Option<PathBuf> {
    let path = percent_encoding::percent_decode_str(url.strip_prefix("file://")?).decode_utf8_lossy();
    Some(PathBuf::from(if cfg!(windows) { path.trim_start_matches('/').replace('/', "\\") } else { path.into_owned() }))
}

/// The name an attachment is kept under in its session's folder.
pub(super) fn kept_name(file: &FilePart) -> String {
    format!("{}.{}", safe(&file.id), extension(file))
}

fn extension(file: &FilePart) -> &str {
    let plain = |s: &&str| !s.is_empty() && s.len() <= 10 && s.chars().all(|c| c.is_ascii_alphanumeric());
    match file.mime.as_str() {
        "image/jpeg" => "jpg",
        "image/svg+xml" => "svg",
        "application/pdf" => "pdf",
        "text/plain" => "txt",
        mime => mime
            .strip_prefix("image/")
            .filter(plain)
            .or_else(|| file.filename.as_deref()?.rsplit_once('.').map(|(_, ext)| ext).filter(plain))
            .unwrap_or("bin"),
    }
}

// Ids come back from the client, so they never reach a path unfiltered.
fn safe(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nabu_protocol::{
        AssistantMessage, Completed, Created, Message, ModelRef, SessionTime, Span, ToolPart,
        UserMessage,
    };
    use ncore::core::ir::{Block, MediaSource};
    use serde_json::json;

    #[test]
    fn attachments_round_trip() {
        let dir = std::env::temp_dir().join(format!("nabucode-store-{}", std::process::id()));
        let store = Store::new(dir.clone());
        let file = FilePart {
            id: "prt_1".into(),
            session_id: "ses_1".into(),
            message_id: "msg_1".into(),
            mime: "image/png".into(),
            filename: Some("a.png".into()),
            url: format!("data:image/png;base64,{}", STANDARD.encode(b"png bytes")),
        };
        let user = UserMessage {
            id: "msg_1".into(),
            session_id: "ses_1".into(),
            time: Created { created: 1 },
            agent: "build".into(),
            model: ModelRef {
                provider_id: "p".into(),
                model_id: "m".into(),
                variant: None,
                fast: false,
            },
        };
        let model = user.model.clone();
        let assistant = AssistantMessage {
            id: "msg_2".into(),
            session_id: user.session_id.clone(),
            parent_id: user.id.clone(),
            provider_id: model.provider_id.clone(),
            model_id: model.model_id.clone(),
            variant: None,
            agent: user.agent.clone(),
            time: Completed {
                created: 1,
                completed: Some(2),
            },
            error: None,
            tokens: Default::default(),
            cost: 0.0,
            finish: Some("tool-calls".into()),
        };
        let mut doc = Doc {
            session: Session {
                id: "ses_1".into(),
                directory: "d".into(),
                title: String::new(),
                time: SessionTime {
                    created: 1,
                    updated: 1,
                    archived: None,
                },
                parent_id: None,
                compactions: Vec::new(),
            },
            messages: vec![
                MessageWithParts {
                    info: Message::User(user),
                    parts: vec![Part::File(file.clone())],
                },
                MessageWithParts {
                    parts: vec![Part::Tool(ToolPart {
                        id: "prt_2".into(),
                        session_id: assistant.session_id.clone(),
                        message_id: assistant.id.clone(),
                        call_id: "call_1".into(),
                        tool: "read".into(),
                        state: ToolState::Completed {
                            input: json!({"path": "a.png"}),
                            output: "Image: a.png".into(),
                            attachments: vec![FilePart {
                                id: "prt_3".into(),
                                message_id: assistant.id.clone(),
                                ..file.clone()
                            }],
                            title: String::new(),
                            metadata: json!({}),
                            time: Span {
                                start: 1,
                                end: Some(2),
                            },
                        },
                        args: r#"{"path":"a.png"}"#.into(),
                        sig: None,
                    })],
                    info: Message::Assistant(assistant),
                },
            ],
            compact: None,
            tools: None,
        };
        store.save(&mut doc);

        assert_eq!(
            std::fs::read(dir.join("ses_1").join("prt_1.png")).unwrap(),
            b"png bytes"
        );
        assert!(
            !std::fs::read_to_string(dir.join("ses_1.json"))
                .unwrap()
                .contains("base64")
        );
        let loaded = store.load_all();
        let Part::File(back) = &loaded[0].messages[0].parts[0] else {
            panic!("file part")
        };
        assert_eq!(back.url, "attachment:prt_1.png");
        let mut history = Vec::new();
        let attachments = super::super::run::Attachments { dir: dir.join("ses_1"), images: true };
        super::super::run::append_history(&mut history, &loaded[0].messages, &model, &attachments);
        let [Block::ToolResult(result)] = history[2].content.as_slice() else {
            panic!("expected a tool result")
        };
        assert_eq!(result.call_id, "call_1");
        assert!(matches!(
            &result.content[1],
            Block::Media { media, .. }
                if media.mime.as_deref() == Some("image/png")
                    && matches!(&media.source, MediaSource::Base64(data) if *data == STANDARD.encode(b"png bytes"))
        ));

        store.remove("ses_1");
        assert!(!dir.join("ses_1").exists() && !dir.join("ses_1.json").exists());
        let _ = std::fs::remove_dir(&dir);
    }

    fn local(id: &str, path: &Path) -> Part {
        Part::File(FilePart {
            id: id.into(),
            session_id: "ses_1".into(),
            message_id: "msg_1".into(),
            mime: "text/plain".into(),
            filename: path.file_name().map(|name| name.to_string_lossy().into_owned()),
            url: format!("file:///{}", path.display().to_string().replace('\\', "/").trim_start_matches('/')),
        })
    }

    #[test]
    fn local_files_are_copied_up_to_the_limit_and_bigger_ones_keep_their_path() {
        let dir = std::env::temp_dir().join(format!("nabucode-local-{}", std::process::id()));
        let source = dir.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let small = source.join("notes.txt");
        std::fs::write(&small, b"hello").unwrap();
        let big = source.join("big.bin");
        std::fs::File::create(&big).unwrap().set_len(COPY_LIMIT + 1).unwrap();
        let store = Store::new(dir.join("sessions"));

        let mut parts = vec![local("prt_1", &small), local("prt_2", &big)];
        store.keep("ses_1", &mut parts);

        let [Part::File(copied), Part::File(kept)] = parts.as_slice() else { panic!("file parts") };
        assert_eq!(copied.url, "attachment:prt_1.txt");
        assert_eq!(std::fs::read(dir.join("sessions/ses_1/prt_1.txt")).unwrap(), b"hello");
        assert_eq!(local_path(&kept.url).unwrap(), big, "a big file stays where it is");
        assert!(!dir.join("sessions/ses_1/prt_2.bin").exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
