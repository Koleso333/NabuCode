mod shells;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use nabu_protocol::{Event, TerminalInfo};
use portable_pty::{ChildKiller, MasterPty, PtySize, native_pty_system};
use tokio::sync::broadcast;

use shells::Shell;

/// Output kept for reconnects, in bytes.
const BACKLOG: usize = 1 << 20;
/// Close code telling the GUI the terminal is gone and reconnecting is useless.
const GONE: u16 = 4404;

pub struct Terminals {
    shells: Vec<Shell>,
    items: Mutex<HashMap<String, Arc<Terminal>>>,
    events: broadcast::Sender<Event>,
}

pub struct Terminal {
    directory: String,
    number: u32,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    input: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output: Mutex<Output>,
    /// `None` once the shell has exited.
    live: broadcast::Sender<Option<String>>,
}

/// Recent output. Positions count UTF-16 units, as the GUI measures strings.
#[derive(Default)]
struct Output {
    text: String,
    start: u64,
    end: u64,
    exited: bool,
}

impl Terminals {
    pub fn new(events: broadcast::Sender<Event>) -> Arc<Self> {
        Arc::new(Self {
            shells: shells::installed(),
            items: Mutex::default(),
            events,
        })
    }

    pub fn shells(&self) -> Vec<nabu_protocol::Shell> {
        self.shells.iter().map(Shell::info).collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Terminal>> {
        self.items.lock().unwrap().get(id).cloned()
    }

    pub fn create(
        self: &Arc<Self>,
        directory: &str,
        shell: Option<&str>,
        highlight: bool,
        size: (u16, u16),
    ) -> Result<TerminalInfo, String> {
        let shell = shell
            .and_then(|id| self.shells.iter().find(|s| s.id == id))
            .or(self.shells.first())
            .ok_or("no shell found")?;
        let pair = native_pty_system()
            .openpty(pty_size(size))
            .map_err(|e| e.to_string())?;
        let mut command = shell.command(highlight);
        command.cwd(directory);
        command.env("TERM", "xterm-256color");
        command.env("COLORTERM", "truecolor");
        let mut child = pair
            .slave
            .spawn_command(command)
            .map_err(|e| format!("cannot start {}: {e}", shell.name))?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let input = pair.master.take_writer().map_err(|e| e.to_string())?;
        let mut items = self.items.lock().unwrap();
        let taken: Vec<u32> = items.values().filter(|t| t.directory == directory).map(|t| t.number).collect();
        let number = (1..).find(|n| !taken.contains(n)).unwrap();
        let terminal = Arc::new(Terminal {
            directory: directory.to_string(),
            number,
            input: Mutex::new(input),
            master: Mutex::new(Some(pair.master)),
            killer: Mutex::new(child.clone_killer()),
            output: Mutex::default(),
            live: broadcast::channel(1024).0,
        });
        let id = crate::sessions::id::ascending("pty");
        items.insert(id.clone(), terminal.clone());
        drop(items);

        let (read_done, reading) = std::sync::mpsc::channel::<()>();
        let reading_terminal = terminal.clone();
        std::thread::spawn(move || {
            reading_terminal.read_output(reader);
            drop(read_done);
        });
        let (terminals, exited_id) = (self.clone(), id.clone());
        std::thread::spawn(move || {
            let _ = child.wait();
            // Closing the pty ends the reader on Windows; a background process may hold it open elsewhere.
            terminal.master.lock().unwrap().take();
            let _ = reading.recv_timeout(Duration::from_secs(1));
            terminal.finish();
            terminals.items.lock().unwrap().remove(&exited_id);
            let _ = terminals
                .events
                .send(Event::TerminalExited { id: exited_id });
        });
        Ok(TerminalInfo { id, number })
    }

    pub fn list(&self, directory: &str) -> Vec<TerminalInfo> {
        let items = self.items.lock().unwrap();
        let mut list: Vec<TerminalInfo> = items
            .iter()
            .filter(|(_, t)| t.directory == directory)
            .map(|(id, t)| TerminalInfo { id: id.clone(), number: t.number })
            .collect();
        // Ids grow with time, so this is the creation order
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub fn resize(&self, id: &str, size: (u16, u16)) -> Result<(), String> {
        let terminal = self.get(id).ok_or("terminal not found")?;
        let master = terminal.master.lock().unwrap();
        master.as_ref().map_or(Ok(()), |master| {
            master.resize(pty_size(size)).map_err(|e| e.to_string())
        })
    }

    /// The exit then goes the usual way: `TerminalExited` and the sockets close.
    pub fn close(&self, id: &str) {
        if let Some(terminal) = self.get(id) {
            let _ = terminal.killer.lock().unwrap().kill();
        }
    }
}

impl Terminal {
    fn read_output(&self, mut reader: Box<dyn Read + Send>) {
        let mut buf = [0u8; 8192];
        let mut pending = Vec::new();
        let mut first = true;
        while let Ok(n @ 1..) = reader.read(&mut buf) {
            pending.extend_from_slice(&buf[..n]);
            let whole = pending.len() - incomplete_tail(&pending);
            let mut text = String::from_utf8_lossy(&pending[..whole]).into_owned();
            pending.drain(..whole);
            if first && !text.is_empty() {
                first = false;
                text = self.answer_cursor_query(text);
            }
            if !text.is_empty() {
                self.push(text);
            }
        }
    }

    /// ConPTY opens by asking where the cursor is. The GUI may answer late or answer again on every
    /// replay, and a late answer lands in the shell as typed text, so the server answers it at once.
    fn answer_cursor_query(&self, text: String) -> String {
        const QUERY: &str = "\x1b[6n";
        if !text.contains(QUERY) {
            return text;
        }
        self.write(b"\x1b[1;1R");
        text.replacen(QUERY, "", 1)
    }

    fn push(&self, text: String) {
        let mut out = self.output.lock().unwrap();
        out.end += utf16_len(&text);
        out.text.push_str(&text);
        if out.text.len() > BACKLOG {
            let mut cut = out.text.len() - BACKLOG / 2;
            while !out.text.is_char_boundary(cut) {
                cut += 1;
            }
            out.start += utf16_len(&out.text[..cut]);
            out.text.drain(..cut);
        }
        // Sent under the lock so `attach` never misses or repeats a chunk.
        let _ = self.live.send(Some(text));
    }

    fn finish(&self) {
        self.output.lock().unwrap().exited = true;
        let _ = self.live.send(None);
    }

    fn write(&self, data: &[u8]) {
        let mut input = self.input.lock().unwrap();
        let _ = input.write_all(data).and_then(|_| input.flush());
    }
}

/// Streams a terminal to the GUI: a `\0{"cursor":N}` frame with the position the text starts at,
/// the output since `cursor` (-1 for nothing old), then live output; text from the GUI is input.
pub async fn stream(socket: WebSocket, terminal: Option<Arc<Terminal>>, cursor: i64) {
    let (mut sink, mut incoming) = socket.split();
    let Some(terminal) = terminal else {
        let _ = sink.send(close(GONE)).await;
        return;
    };
    let (from, replay, exited, mut live) = {
        let out = terminal.output.lock().unwrap();
        let from = if cursor < 0 || cursor as u64 > out.end {
            out.end
        } else {
            (cursor as u64).max(out.start)
        };
        (
            from,
            utf16_suffix(&out.text, from - out.start).to_string(),
            out.exited,
            terminal.live.subscribe(),
        )
    };
    let meta = [
        b"\0".as_slice(),
        format!(r#"{{"cursor":{from}}}"#).as_bytes(),
    ]
    .concat();
    if sink.send(Message::Binary(meta.into())).await.is_err() {
        return;
    }
    if !replay.is_empty() && sink.send(Message::Text(replay.into())).await.is_err() {
        return;
    }
    if exited {
        let _ = sink.send(close(GONE)).await;
        return;
    }
    loop {
        tokio::select! {
            chunk = live.recv() => match chunk {
                Ok(Some(text)) => if sink.send(Message::Text(text.into())).await.is_err() { return },
                Ok(None) | Err(broadcast::error::RecvError::Closed) => {
                    let _ = sink.send(close(GONE)).await;
                    return;
                }
                // The GUI reconnects from its cursor and gets the missed part from the backlog.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    let _ = sink.send(close(1011)).await;
                    return;
                }
            },
            message = incoming.next() => match message {
                Some(Ok(Message::Text(text))) => terminal.write(text.as_bytes()),
                Some(Ok(Message::Binary(bytes))) => terminal.write(&bytes),
                Some(Ok(_)) => {}
                _ => return,
            },
        }
    }
}

fn close(code: u16) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: "".into(),
    }))
}

fn pty_size((cols, rows): (u16, u16)) -> PtySize {
    PtySize {
        cols: cols.max(1),
        rows: rows.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn utf16_len(text: &str) -> u64 {
    text.chars().map(|c| c.len_utf16() as u64).sum()
}

fn utf16_suffix(text: &str, skip: u64) -> &str {
    let mut seen = 0;
    for (index, c) in text.char_indices() {
        if seen >= skip {
            return &text[index..];
        }
        seen += c.len_utf16() as u64;
    }
    ""
}

/// Bytes at the end that start a UTF-8 character the next read will finish.
fn incomplete_tail(bytes: &[u8]) -> usize {
    for back in 1..=bytes.len().min(3) {
        let byte = bytes[bytes.len() - back];
        if byte & 0xC0 != 0x80 {
            let needs = match byte {
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                _ => 1,
            };
            return if needs > back { back } else { 0 };
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_characters_wait_for_the_rest() {
        let bytes = "aж€".as_bytes();
        assert_eq!(incomplete_tail(bytes), 0);
        assert_eq!(incomplete_tail(&bytes[..bytes.len() - 1]), 2);
        assert_eq!(incomplete_tail(&bytes[..2]), 1);
    }

    #[test]
    fn suffix_counts_utf16() {
        assert_eq!(utf16_suffix("a😀b", 3), "b");
        assert_eq!(utf16_suffix("ab", 5), "");
    }
}
