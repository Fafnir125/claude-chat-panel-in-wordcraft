//! The chat's backend: Claude Code (`claude -p`) on this machine, with the user's own sign-in, so
//! no API key. Each message runs one `claude` process with streamed JSON output; follow-ups
//! `--resume` its session. Claude reaches the open document only through WordCraft's MCP server
//! bridged to this window's control port, and only through the read and propose tools.

use serde_json::Value;

/// What the chat hears from a running reply.
#[derive(Clone, Debug, PartialEq)]
pub enum ChatEvent {
    /// The session id, once Claude Code reports it.
    Session(String),
    /// More reply text.
    Delta(String),
    /// Claude is using a tool (its short name), shown as a status line.
    Tool(String),
    /// The reply finished.
    Done,
    /// The reply failed; the message is for the user.
    Error(String),
}

/// The MCP tools Claude may use: read and propose, never direct edits.
pub const ALLOWED_TOOLS: &str = "mcp__wordcraft__read_document,mcp__wordcraft__propose_edits";

/// Events from one line of `--output-format stream-json`. Unknown lines give none.
pub fn parse_line(line: &str) -> Vec<ChatEvent> {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return Vec::new() };
    let mut out = Vec::new();
    match v.get("type").and_then(Value::as_str) {
        Some("system") => {
            if let Some(id) = v.get("session_id").and_then(Value::as_str) {
                out.push(ChatEvent::Session(id.to_string()));
            }
        }
        Some("stream_event") => {
            let ev = &v["event"];
            if ev["type"] == "content_block_delta"
                && ev["delta"]["type"] == "text_delta"
                && let Some(t) = ev["delta"]["text"].as_str()
            {
                out.push(ChatEvent::Delta(t.to_string()));
            }
            if ev["type"] == "content_block_start"
                && ev["content_block"]["type"] == "tool_use"
                && let Some(n) = ev["content_block"]["name"].as_str()
            {
                out.push(ChatEvent::Tool(n.rsplit("__").next().unwrap_or(n).to_string()));
            }
        }
        Some("result") => {
            if let Some(id) = v.get("session_id").and_then(Value::as_str) {
                out.push(ChatEvent::Session(id.to_string()));
            }
            if v.get("is_error").and_then(Value::as_bool).unwrap_or(false) {
                let msg = v.get("result").and_then(Value::as_str).unwrap_or("Claude Code meldete einen Fehler.");
                out.push(ChatEvent::Error(msg.chars().take(500).collect()));
            } else {
                out.push(ChatEvent::Done);
            }
        }
        _ => {}
    }
    out
}

/// The instructions appended to Claude Code's system prompt.
pub fn system_prompt(outline: &str, selection: Option<&str>) -> String {
    let mut s = String::from(
        "Du bist Claude im Chat-Panel des Textverarbeitungsprogramms WordCraft. Antworte knapp, auf Deutsch, \
         sofern die Nutzerin oder der Nutzer nicht anders schreibt. Das offene Dokument erreichst du nur über die \
         Werkzeuge read_document (lesen) und propose_edits (Änderungen vorschlagen). Du änderst das Dokument nie \
         direkt: propose_edits zeigt einen Vorschlag, den der Mensch mit „Übernehmen“ bestätigt. Schlage alle \
         Änderungen einer Bitte in einem einzigen propose_edits-Aufruf vor und beschreibe in einem Satz, was du \
         vorschlägst. Lies vor einem Vorschlag den betroffenen Text mit read_document.\n\nGliederung des Dokuments:\n",
    );
    s.push_str(outline);
    if let Some(sel) = selection {
        s.push_str(
            "\n\nDie Nachricht bezieht sich auf diesen markierten Abschnitt. Beschränke Änderungen auf ihn, \
             außer es wird ausdrücklich etwas anderes verlangt:\n<auswahl>\n",
        );
        s.push_str(sel);
        s.push_str("\n</auswahl>");
    }
    s
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::{Reply, start};

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::io::{BufRead, BufReader};
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{Receiver, channel};

    use super::{ALLOWED_TOOLS, ChatEvent, parse_line};

    /// A running reply: its events and the process, killed when dropped.
    pub struct Reply {
        pub rx: Receiver<ChatEvent>,
        child: Option<Child>,
        _config: Option<tempfile_guard::Guard>,
    }

    impl Drop for Reply {
        fn drop(&mut self) {
            if let Some(c) = self.child.as_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }

    mod tempfile_guard {
        /// Removes the MCP config file when the reply ends.
        pub struct Guard(pub std::path::PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
    }

    /// `claude`: `WORDCRAFT_CLAUDE`, then `~/.local/bin/claude`, then the PATH.
    fn claude_exe() -> PathBuf {
        if let Some(p) = std::env::var_os("WORDCRAFT_CLAUDE") {
            return PathBuf::from(p);
        }
        if let Some(home) = std::env::var_os("HOME") {
            let p = PathBuf::from(home).join(".local/bin/claude");
            if p.is_file() {
                return p;
            }
        }
        PathBuf::from("claude")
    }

    /// `wordcraft-cli`: `WORDCRAFT_CLI`, then beside this executable, then the PATH.
    fn cli_exe() -> PathBuf {
        if let Some(p) = std::env::var_os("WORDCRAFT_CLI") {
            return PathBuf::from(p);
        }
        let name = if cfg!(windows) { "wordcraft-cli.exe" } else { "wordcraft-cli" };
        if let Some(p) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(name)))
            && p.is_file()
        {
            return p;
        }
        PathBuf::from(name)
    }

    /// Start one reply to `prompt`, with Claude bridged to the control port `port`.
    pub fn start(prompt: &str, system: &str, session: Option<&str>, port: u16, ctx: egui::Context) -> Reply {
        let (tx, rx) = channel();
        let cfg = serde_json::json!({"mcpServers": {"wordcraft": {"command": cli_exe(), "args": ["mcp", "--connect", format!("127.0.0.1:{port}")]}}});
        let path = std::env::temp_dir().join(format!("wordcraft-claude-mcp-{}-{port}.json", std::process::id()));
        if let Err(e) = std::fs::write(&path, cfg.to_string()) {
            let _ = tx.send(ChatEvent::Error(format!("Die MCP-Konfiguration ließ sich nicht schreiben: {e}")));
            return Reply { rx, child: None, _config: None };
        }
        let guard = tempfile_guard::Guard(path.clone());
        let mut cmd = Command::new(claude_exe());
        cmd.arg("-p")
            .arg(prompt)
            .args(["--output-format", "stream-json", "--verbose", "--include-partial-messages"])
            .arg("--mcp-config")
            .arg(&path)
            .arg("--strict-mcp-config")
            .args(["--tools", ""])
            .args(["--allowedTools", ALLOWED_TOOLS])
            .arg("--append-system-prompt")
            .arg(system);
        if let Some(id) = session {
            cmd.args(["--resume", id]);
        }
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let msg = if e.kind() == std::io::ErrorKind::NotFound {
                    "Claude Code wurde nicht gefunden. Installiere es und melde dich mit „claude“ an.".to_string()
                } else {
                    format!("Claude Code ließ sich nicht starten: {e}")
                };
                let _ = tx.send(ChatEvent::Error(msg));
                return Reply { rx, child: None, _config: Some(guard) };
            }
        };
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let err_tx = tx.clone();
        std::thread::spawn(move || {
            let mut finished = false;
            if let Some(out) = stdout {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    for ev in parse_line(&line) {
                        finished |= matches!(ev, ChatEvent::Done | ChatEvent::Error(_));
                        if tx.send(ev).is_err() {
                            return;
                        }
                    }
                    ctx.request_repaint();
                }
            }
            if !finished {
                let mut msg = String::new();
                if let Some(e) = stderr {
                    for l in BufReader::new(e).lines().map_while(Result::ok).take(5) {
                        msg.push_str(&l);
                        msg.push('\n');
                    }
                }
                let msg =
                    if msg.trim().is_empty() { "Claude Code hat ohne Antwort beendet.".to_string() } else { msg.trim().chars().take(500).collect() };
                let _ = err_tx.send(ChatEvent::Error(msg));
            }
            ctx.request_repaint();
        });
        Reply { rx, child: Some(child), _config: Some(guard) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_streamed_text_tools_and_result() {
        let lines = [
            r#"{"type":"system","subtype":"init","session_id":"abc"}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hal"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","name":"mcp__wordcraft__propose_edits"}}}"#,
            r#"{"type":"result","subtype":"success","is_error":false,"session_id":"abc","result":"Hallo"}"#,
            "not json",
        ];
        let evs: Vec<ChatEvent> = lines.iter().flat_map(|l| parse_line(l)).collect();
        assert_eq!(
            evs,
            vec![
                ChatEvent::Session("abc".into()),
                ChatEvent::Delta("Hal".into()),
                ChatEvent::Tool("propose_edits".into()),
                ChatEvent::Session("abc".into()),
                ChatEvent::Done
            ]
        );
    }

    #[test]
    fn an_error_result_is_reported() {
        let evs = parse_line(r#"{"type":"result","is_error":true,"result":"nicht angemeldet"}"#);
        assert_eq!(evs, vec![ChatEvent::Error("nicht angemeldet".into())]);
    }

    #[test]
    fn the_selection_is_explicit_context() {
        let p = system_prompt("1. Einleitung", Some("Der Hund"));
        assert!(p.contains("<auswahl>\nDer Hund\n</auswahl>"));
        assert!(!system_prompt("x", None).contains("<auswahl>"));
    }
}
