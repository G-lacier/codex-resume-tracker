use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::app_server::{MessageRole, VisibleMessage};
use crate::config::{Config, SummaryInput, SummaryProvider};
use crate::paths::AppPaths;
use crate::VERSION;

pub const MAX_OUTPUT_CHARS: usize = 240;
const OPENAI_RESPONSES_URL: &str = "https://api.openai.com/v1/responses";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SummaryResult {
    pub title: String,
    pub summary: String,
}

impl SummaryResult {
    pub fn normalized(self) -> Result<Self> {
        let title = normalize_output(&self.title);
        let summary = normalize_output(&self.summary);
        if title.is_empty() {
            bail!("summary provider returned an empty title");
        }
        if summary.is_empty() {
            bail!("summary provider returned an empty summary");
        }
        Ok(Self { title, summary })
    }
}

pub fn generate(
    provider: SummaryProvider,
    cap: SummaryInput,
    messages: &[VisibleMessage],
    cwd: &Path,
    config: &Config,
    paths: &AppPaths,
) -> Result<SummaryResult> {
    match provider {
        SummaryProvider::Local => local_summary(messages),
        SummaryProvider::Codex => {
            let transcript = format_messages(messages, cap);
            codex_summary(&transcript, cwd, paths)
        }
        SummaryProvider::Openai => {
            let transcript = format_messages(messages, cap);
            openai_summary(&transcript, &config.openai_model)
        }
    }
}

pub fn local_summary(messages: &[VisibleMessage]) -> Result<SummaryResult> {
    let title = messages
        .iter()
        .find(|message| message.role == MessageRole::User)
        .or_else(|| messages.first())
        .map(|message| message.text.as_str())
        .unwrap_or("Untitled Codex session");
    let summary = messages
        .last()
        .map(|message| message.text.as_str())
        .unwrap_or("No visible user or assistant messages were available.");
    SummaryResult {
        title: title.to_owned(),
        summary: summary.to_owned(),
    }
    .normalized()
}

pub fn normalize_output(value: &str) -> String {
    let collapsed = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = collapsed.chars().count();
    if count <= MAX_OUTPUT_CHARS {
        return collapsed;
    }
    let mut shortened: String = collapsed.chars().take(MAX_OUTPUT_CHARS - 1).collect();
    shortened.push('…');
    shortened
}

pub fn format_messages(messages: &[VisibleMessage], cap: SummaryInput) -> String {
    let rendered: Vec<String> = messages
        .iter()
        .map(|message| format!("{}: {}", message.role.label(), message.text.trim()))
        .collect();
    let complete = rendered.join("\n\n");
    let Some(limit) = cap.character_limit() else {
        return complete;
    };
    if complete.chars().count() <= limit {
        return complete;
    }
    representative_sample(messages, &complete, limit)
}

fn representative_sample(messages: &[VisibleMessage], complete: &str, limit: usize) -> String {
    if limit == 0 || messages.is_empty() {
        return String::new();
    }
    let desired = messages.len().min(25);
    let mut indices = quantile_indices(messages.len(), desired);
    let anchors = quantile_indices(messages.len(), messages.len().min(3));

    loop {
        let overhead = indices
            .iter()
            .map(|index| {
                format!(
                    "[message {}/{}] {}: ",
                    index + 1,
                    messages.len(),
                    messages[*index].role.label()
                )
                .chars()
                .count()
                    + 2
            })
            .sum::<usize>();
        if overhead + indices.len() <= limit || indices.len() <= anchors.len() {
            break;
        }
        let removable = indices
            .iter()
            .enumerate()
            .rev()
            .find(|(_, index)| !anchors.contains(index))
            .map(|(position, _)| position);
        if let Some(position) = removable {
            indices.remove(position);
        } else {
            break;
        }
    }

    let labels: Vec<String> = indices
        .iter()
        .map(|index| {
            format!(
                "[message {}/{}] {}: ",
                index + 1,
                messages.len(),
                messages[*index].role.label()
            )
        })
        .collect();
    let overhead = labels
        .iter()
        .map(|label| label.chars().count())
        .sum::<usize>()
        + indices.len().saturating_sub(1) * 2;
    if overhead + indices.len() > limit {
        return raw_beginning_middle_end(complete, limit);
    }

    let available = limit - overhead;
    let base = available / indices.len();
    let remainder = available % indices.len();
    let mut output = String::new();
    for (position, index) in indices.iter().enumerate() {
        if position > 0 {
            output.push_str("\n\n");
        }
        output.push_str(&labels[position]);
        let quota = base + usize::from(position < remainder);
        output.push_str(&truncate_chars(messages[*index].text.trim(), quota));
    }
    debug_assert!(output.chars().count() <= limit);
    output
}

fn quantile_indices(length: usize, desired: usize) -> Vec<usize> {
    if length == 0 || desired == 0 {
        return Vec::new();
    }
    if desired == 1 {
        return vec![0];
    }
    let mut indices = Vec::new();
    for position in 0..desired {
        let index = position * (length - 1) / (desired - 1);
        if indices.last().copied() != Some(index) {
            indices.push(index);
        }
    }
    indices
}

fn raw_beginning_middle_end(value: &str, limit: usize) -> String {
    let characters: Vec<char> = value.chars().collect();
    if characters.len() <= limit {
        return value.to_owned();
    }
    if limit <= 2 {
        return characters.into_iter().take(limit).collect();
    }
    let content_budget = limit - 2;
    let first = content_budget / 3;
    let middle = content_budget / 3;
    let last = content_budget - first - middle;
    let center = characters.len() / 2;
    let middle_start = center.saturating_sub(middle / 2);
    let mut output = String::new();
    output.extend(characters.iter().take(first));
    output.push('…');
    output.extend(characters.iter().skip(middle_start).take(middle));
    output.push('…');
    output.extend(characters.iter().skip(characters.len() - last));
    output
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    match limit {
        0 => String::new(),
        1 => "…".to_owned(),
        _ => {
            let mut output: String = value.chars().take(limit - 1).collect();
            output.push('…');
            output
        }
    }
}

fn provider_prompt(transcript: &str) -> String {
    format!(
        "Create a concise title and summary for this Codex thread. Use only the visible user and assistant messages below. Do not infer hidden reasoning or tool output. Both fields must be at most {MAX_OUTPUT_CHARS} Unicode characters. Return only the requested JSON object.\n\n<transcript>\n{transcript}\n</transcript>"
    )
}

pub fn output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "title": {"type": "string", "maxLength": MAX_OUTPUT_CHARS},
            "summary": {"type": "string", "maxLength": MAX_OUTPUT_CHARS}
        },
        "required": ["title", "summary"]
    })
}

pub fn codex_exec_args(cwd: &Path, schema_path: &Path) -> Vec<String> {
    vec![
        "exec".into(),
        "--ephemeral".into(),
        "--ignore-user-config".into(),
        "--ignore-rules".into(),
        "--disable".into(),
        "hooks".into(),
        "--sandbox".into(),
        "read-only".into(),
        "--skip-git-repo-check".into(),
        "--output-schema".into(),
        schema_path.to_string_lossy().into_owned(),
        "--color".into(),
        "never".into(),
        "-C".into(),
        cwd.to_string_lossy().into_owned(),
        "-".into(),
    ]
}

fn codex_summary(transcript: &str, cwd: &Path, paths: &AppPaths) -> Result<SummaryResult> {
    paths.ensure_dirs()?;
    let schema_path = paths.summary_schema();
    fs::write(&schema_path, serde_json::to_vec_pretty(&output_schema())?)
        .with_context(|| format!("write summary schema {}", schema_path.display()))?;
    let args = codex_exec_args(cwd, &schema_path);
    let mut child = Command::new("codex")
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("start ephemeral Codex summarizer")?;
    child
        .stdin
        .as_mut()
        .context("ephemeral Codex stdin unavailable")?
        .write_all(provider_prompt(transcript).as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ephemeral Codex summarizer exited with {}: {}",
            output.status,
            truncate_chars(stderr.trim(), 1000)
        );
    }
    let stdout = String::from_utf8(output.stdout).context("Codex summary was not UTF-8")?;
    parse_structured_output(&stdout).context("parse ephemeral Codex structured output")
}

pub fn build_openai_request(model: &str, transcript: &str) -> Value {
    json!({
        "model": model,
        "store": false,
        "instructions": format!(
            "Generate a faithful Codex thread title and summary. Each must be at most {MAX_OUTPUT_CHARS} Unicode characters."
        ),
        "input": [{
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": provider_prompt(transcript)
            }]
        }],
        "text": {
            "format": {
                "type": "json_schema",
                "name": "codex_thread_summary",
                "strict": true,
                "schema": output_schema()
            }
        }
    })
}

fn openai_summary(transcript: &str, model: &str) -> Result<SummaryResult> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .context("OPENAI_API_KEY is required for the OpenAI summary provider")?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent(format!("codex-resume-tracker/{VERSION}"))
        .build()?;
    let response = client
        .post(OPENAI_RESPONSES_URL)
        .bearer_auth(api_key)
        .json(&build_openai_request(model, transcript))
        .send()
        .context("call OpenAI Responses API")?;
    let status = response.status();
    let body = response.text().context("read OpenAI Responses API body")?;
    if !status.is_success() {
        bail!(
            "OpenAI Responses API returned {status}: {}",
            truncate_chars(&body, 1000)
        );
    }
    let response: Value = serde_json::from_str(&body).context("parse OpenAI Responses API JSON")?;
    parse_openai_response(&response)
}

pub fn parse_openai_response(response: &Value) -> Result<SummaryResult> {
    if response.get("status").and_then(Value::as_str) != Some("completed") {
        let error = response
            .get("error")
            .map(|value| value.to_string())
            .unwrap_or_else(|| "response was not completed".to_owned());
        bail!("OpenAI response did not complete: {error}");
    }
    let text = response
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .find(|part| part.get("type").and_then(Value::as_str) == Some("output_text"))
        .and_then(|part| part.get("text"))
        .and_then(Value::as_str)
        .context("OpenAI response omitted assistant output_text")?;
    parse_structured_output(text).context("parse OpenAI structured output")
}

pub fn parse_structured_output(value: &str) -> Result<SummaryResult> {
    if let Ok(result) = serde_json::from_str::<SummaryResult>(value.trim()) {
        return result.normalized();
    }
    for line in value.lines().rev() {
        if let Ok(result) = serde_json::from_str::<SummaryResult>(line.trim()) {
            return result.normalized();
        }
    }
    bail!("provider output was not a title/summary JSON object")
}

pub fn schema_path_for(paths: &AppPaths) -> PathBuf {
    paths.summary_schema()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: MessageRole, text: &str) -> VisibleMessage {
        VisibleMessage {
            role,
            text: text.into(),
        }
    }

    #[test]
    fn local_contract_uses_first_request_and_latest_visible_message() {
        let messages = vec![
            message(MessageRole::User, "Build the tracker"),
            message(MessageRole::Assistant, "I will inspect the repository."),
            message(MessageRole::User, "Also add tags"),
            message(MessageRole::Assistant, "Tags are implemented."),
        ];
        let result = local_summary(&messages).unwrap();
        assert_eq!(result.title, "Build the tracker");
        assert_eq!(result.summary, "Tags are implemented.");
    }

    #[test]
    fn normalization_is_whitespace_clean_and_unicode_bounded() {
        let input = format!("  hello\n\tworld   {}", "🦀".repeat(300));
        let output = normalize_output(&input);
        assert!(output.starts_with("hello world "));
        assert_eq!(output.chars().count(), MAX_OUTPUT_CHARS);
        assert!(output.ends_with('…'));
    }

    #[test]
    fn capped_sampling_retains_beginning_middle_and_end() {
        let mut messages = Vec::new();
        for index in 0..30 {
            let sentinel = match index {
                0 => "BEGIN_SENTINEL",
                14 => "MIDDLE_SENTINEL",
                29 => "END_SENTINEL",
                _ => "ordinary",
            };
            messages.push(message(
                if index % 2 == 0 {
                    MessageRole::User
                } else {
                    MessageRole::Assistant
                },
                &format!("{sentinel} {}", "x".repeat(100)),
            ));
        }
        let sampled = format_messages(&messages, SummaryInput::Custom(1200));
        assert!(sampled.contains("BEGIN_SENTINEL"));
        assert!(sampled.contains("MIDDLE_SENTINEL"));
        assert!(sampled.contains("END_SENTINEL"));
        assert!(sampled.chars().count() <= 1200);
    }

    #[test]
    fn entire_policy_does_not_truncate() {
        let messages = vec![message(MessageRole::User, &"z".repeat(80_000))];
        assert_eq!(
            format_messages(&messages, SummaryInput::Entire)
                .chars()
                .count(),
            80_006
        );
    }

    #[test]
    fn codex_contract_is_ephemeral_read_only_and_hook_free() {
        let args = codex_exec_args(Path::new("/workspace"), Path::new("schema.json"));
        let joined = args.join(" ");
        assert!(joined.contains("--ephemeral"));
        assert!(joined.contains("--ignore-user-config"));
        assert!(joined.contains("--disable hooks"));
        assert!(joined.contains("--sandbox read-only"));
        assert!(joined.contains("--output-schema schema.json"));
        assert!(joined.contains("-C /workspace"));
    }

    #[test]
    fn openai_contract_uses_responses_structured_output_without_storage() {
        let request = build_openai_request("gpt-5-mini", "User: hello");
        assert_eq!(request["model"], "gpt-5-mini");
        assert_eq!(request["store"], false);
        assert_eq!(request["text"]["format"]["type"], "json_schema");
        assert_eq!(request["text"]["format"]["strict"], true);
        assert_eq!(
            request["text"]["format"]["schema"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn openai_response_contract_extracts_structured_output() {
        let response = json!({
            "status": "completed",
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "{\"title\":\"A title\",\"summary\":\"A summary\"}"
                }]
            }]
        });
        assert_eq!(
            parse_openai_response(&response).unwrap(),
            SummaryResult {
                title: "A title".into(),
                summary: "A summary".into()
            }
        );
    }

    #[test]
    fn provider_errors_do_not_get_reinterpreted_as_another_contract() {
        let response = json!({"status":"failed","error":{"message":"quota"},"output":[]});
        let error = parse_openai_response(&response).unwrap_err().to_string();
        assert!(error.contains("quota"));
    }
}
