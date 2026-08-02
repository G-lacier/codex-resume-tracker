use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::config::SourceKind;
use crate::db::SessionMetadata;
use crate::VERSION;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MessageRole {
    User,
    Assistant,
}

impl MessageRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::User => "User",
            Self::Assistant => "Assistant",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisibleMessage {
    pub role: MessageRole,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteThread {
    pub metadata: SessionMetadata,
}

pub trait ThreadRepository {
    fn list_threads(&mut self, sources: &[SourceKind]) -> Result<Vec<RemoteThread>>;
    fn read_messages(&mut self, thread_id: &str) -> Result<Vec<VisibleMessage>>;
}

pub struct CodexAppServer {
    child: Child,
    input: BufWriter<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl CodexAppServer {
    pub fn connect() -> Result<Self> {
        Self::connect_with_program("codex")
    }

    pub fn connect_with_program(program: &str) -> Result<Self> {
        let mut child = Command::new(program)
            .args(["app-server", "--listen", "stdio://"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("start {program} app-server"))?;
        let input = BufWriter::new(child.stdin.take().context("app-server stdin unavailable")?);
        let output = BufReader::new(
            child
                .stdout
                .take()
                .context("app-server stdout unavailable")?,
        );
        let mut server = Self {
            child,
            input,
            output,
            next_id: 1,
        };
        server.initialize()?;
        Ok(server)
    }

    fn initialize(&mut self) -> Result<()> {
        self.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "codex_resume_tracker",
                    "title": "Codex Resume Tracker",
                    "version": VERSION
                }
            }),
        )?;
        self.notification("initialized", json!({}))?;
        Ok(())
    }

    fn notification(&mut self, method: &str, params: Value) -> Result<()> {
        let message = json!({"method": method, "params": params});
        serde_json::to_writer(&mut self.input, &message)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({"method": method, "id": id, "params": params});
        serde_json::to_writer(&mut self.input, &message)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;

        loop {
            let mut line = String::new();
            let bytes = self.output.read_line(&mut line)?;
            if bytes == 0 {
                let status = self.child.try_wait()?.map(|value| value.to_string());
                bail!(
                    "Codex App Server closed before replying to {method}{}",
                    status
                        .map(|value| format!(" (status {value})"))
                        .unwrap_or_default()
                );
            }
            let envelope: Value = serde_json::from_str(&line)
                .with_context(|| format!("parse App Server response: {line:?}"))?;
            if envelope.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = envelope.get("error") {
                bail!("App Server {method} failed: {}", compact_json(error));
            }
            return envelope
                .get("result")
                .cloned()
                .ok_or_else(|| anyhow!("App Server {method} response omitted result"));
        }
    }

    pub fn validate_connection(&mut self) -> Result<()> {
        let _ = self.request(
            "thread/list",
            json!({
                "limit": 1,
                "archived": false,
                "sourceKinds": ["cli", "vscode"]
            }),
        )?;
        Ok(())
    }
}

impl ThreadRepository for CodexAppServer {
    fn list_threads(&mut self, sources: &[SourceKind]) -> Result<Vec<RemoteThread>> {
        let mut cursor: Option<String> = None;
        let mut threads = Vec::new();
        loop {
            let result = self.request(
                "thread/list",
                thread_list_params(sources, cursor.as_deref()),
            )?;
            let (mut page, next_cursor) = parse_thread_list_page(&result)?;
            threads.append(&mut page);
            match next_cursor {
                Some(next) if Some(next.as_str()) != cursor.as_deref() => cursor = Some(next),
                Some(_) => bail!("App Server returned a repeated thread/list cursor"),
                None => break,
            }
        }
        Ok(threads)
    }

    fn read_messages(&mut self, thread_id: &str) -> Result<Vec<VisibleMessage>> {
        let result = self.request(
            "thread/read",
            json!({"threadId": thread_id, "includeTurns": true}),
        )?;
        extract_visible_messages(&result)
    }
}

impl Drop for CodexAppServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn thread_list_params(sources: &[SourceKind], cursor: Option<&str>) -> Value {
    let sources: Vec<&str> = sources
        .iter()
        .map(|source| source.as_protocol_str())
        .collect();
    let mut params = json!({
        "limit": 100,
        "archived": false,
        "sortKey": "recency_at",
        "sortDirection": "desc",
        "sourceKinds": sources,
    });
    if let Some(cursor) = cursor {
        params["cursor"] = Value::String(cursor.to_owned());
    }
    params
}

pub fn parse_thread_list_page(result: &Value) -> Result<(Vec<RemoteThread>, Option<String>)> {
    let data = result
        .get("data")
        .and_then(Value::as_array)
        .context("thread/list result omitted data array")?;
    let mut threads = Vec::with_capacity(data.len());
    for item in data {
        let thread_id = string_field(item, "id")?;
        let cwd = string_field(item, "cwd")?;
        let source = item
            .get("source")
            .map(source_label)
            .unwrap_or_else(|| "unknown".to_owned());
        let title = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .or_else(|| item.get("preview").and_then(Value::as_str))
            .map(str::to_owned);
        threads.push(RemoteThread {
            metadata: SessionMetadata {
                thread_id,
                cwd,
                source,
                model: item.get("model").and_then(Value::as_str).map(str::to_owned),
                model_provider: item
                    .get("modelProvider")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                created_at: item.get("createdAt").and_then(Value::as_i64),
                updated_at: item.get("updatedAt").and_then(Value::as_i64),
                recency_at: item.get("recencyAt").and_then(Value::as_i64),
                generated_title: title,
            },
        });
    }
    let cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Ok((threads, cursor))
}

pub fn extract_visible_messages(result: &Value) -> Result<Vec<VisibleMessage>> {
    let thread = result
        .get("thread")
        .context("thread/read result omitted thread")?;
    let turns = thread
        .get("turns")
        .and_then(Value::as_array)
        .context("thread/read result omitted turns array")?;
    let mut messages = Vec::new();
    for turn in turns {
        let Some(items) = turn.get("items").and_then(Value::as_array) else {
            continue;
        };
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("userMessage") => {
                    let text = item
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|input| input.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|input| input.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n");
                    if !text.trim().is_empty() {
                        messages.push(VisibleMessage {
                            role: MessageRole::User,
                            text,
                        });
                    }
                }
                Some("agentMessage") => {
                    if let Some(text) = item
                        .get("text")
                        .and_then(Value::as_str)
                        .filter(|text| !text.trim().is_empty())
                    {
                        messages.push(VisibleMessage {
                            role: MessageRole::Assistant,
                            text: text.to_owned(),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    Ok(messages)
}

fn source_label(value: &Value) -> String {
    if let Some(value) = value.as_str() {
        return value.to_owned();
    }
    if let Some(custom) = value.get("custom").and_then(Value::as_str) {
        return format!("custom:{custom}");
    }
    if let Some(subagent) = value.get("subAgent") {
        if let Some(kind) = subagent.as_str() {
            return format!("subAgent:{kind}");
        }
        return "subAgent".to_owned();
    }
    "unknown".to_owned()
}

fn string_field(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .with_context(|| format!("thread/list item omitted string field {field}"))
}

fn compact_json(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "unknown error".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_filters_are_explicit_and_exec_is_opt_in() {
        let interactive = thread_list_params(&[SourceKind::Cli, SourceKind::Vscode], None);
        assert_eq!(interactive["sourceKinds"], json!(["cli", "vscode"]));
        assert_eq!(interactive.get("cursor"), None);

        let all = thread_list_params(
            &[SourceKind::Cli, SourceKind::Vscode, SourceKind::Exec],
            Some("next-page"),
        );
        assert_eq!(all["sourceKinds"], json!(["cli", "vscode", "exec"]));
        assert_eq!(all["cursor"], "next-page");
        assert_eq!(all["archived"], false);
    }

    #[test]
    fn list_page_parses_metadata_and_cursor() {
        let fixture = json!({
            "data": [{
                "id": "thr_123",
                "cwd": "/repo",
                "source": "cli",
                "modelProvider": "openai",
                "model": "gpt-5",
                "createdAt": 10,
                "updatedAt": 20,
                "recencyAt": 19,
                "preview": "Build a tracker"
            }],
            "nextCursor": "opaque"
        });
        let (threads, cursor) = parse_thread_list_page(&fixture).unwrap();
        assert_eq!(cursor.as_deref(), Some("opaque"));
        assert_eq!(threads[0].metadata.thread_id, "thr_123");
        assert_eq!(threads[0].metadata.model.as_deref(), Some("gpt-5"));
        assert_eq!(
            threads[0].metadata.generated_title.as_deref(),
            Some("Build a tracker")
        );
    }

    #[test]
    fn extraction_keeps_only_visible_user_and_agent_text() {
        let fixture = json!({
            "thread": {
                "turns": [{
                    "items": [
                        {"type":"userMessage","id":"u1","content":[
                            {"type":"text","text":"visible request"},
                            {"type":"localImage","path":"secret.png"}
                        ]},
                        {"type":"reasoning","id":"r1","summary":["private chain"]},
                        {"type":"commandExecution","id":"c1","aggregatedOutput":"tool secret"},
                        {"type":"agentMessage","id":"a1","text":"visible answer"},
                        {"type":"hookPrompt","id":"h1","fragments":[]}
                    ]
                }]
            }
        });
        let messages = extract_visible_messages(&fixture).unwrap();
        assert_eq!(
            messages,
            vec![
                VisibleMessage {
                    role: MessageRole::User,
                    text: "visible request".into()
                },
                VisibleMessage {
                    role: MessageRole::Assistant,
                    text: "visible answer".into()
                },
            ]
        );
        let joined = messages
            .iter()
            .map(|message| message.text.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!joined.contains("private chain"));
        assert!(!joined.contains("tool secret"));
    }

    #[test]
    fn custom_and_subagent_sources_are_labeled_without_panics() {
        assert_eq!(source_label(&json!({"custom":"desktop"})), "custom:desktop");
        assert_eq!(
            source_label(&json!({"subAgent":"review"})),
            "subAgent:review"
        );
        assert_eq!(
            source_label(&json!({"subAgent":{"thread_spawn":{}}})),
            "subAgent"
        );
    }
}
