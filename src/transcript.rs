use std::fs::{self, File, OpenOptions};
use std::io::Write;
#[cfg(test)]
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::approval::ApprovalDecision;
use crate::nexus::ToolCall;
use crate::ui::{Ui, UiEvent};

pub(crate) struct TranscriptLog {
    file: File,
}

impl TranscriptLog {
    pub(crate) fn open_if_enabled() -> Result<Option<Self>> {
        if transcript_enabled() {
            Self::open_default().map(Some)
        } else {
            Ok(None)
        }
    }

    fn open_default() -> Result<Self> {
        Self::open_at(crate::paths::transcript_path()?)
    }

    pub(crate) fn open_at(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!("failed to create transcript directory {}", parent.display())
            })?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options
            .open(&path)
            .with_context(|| format!("failed to open transcript {}", path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).with_context(|| {
                format!("failed to set transcript permissions {}", path.display())
            })?;
        }
        Ok(Self { file })
    }

    pub(crate) fn log_prompt(&mut self, prompt: &str) -> Result<()> {
        self.append("user", &redact_text(prompt))
    }

    pub(crate) fn log_event(&mut self, event: &UiEvent) -> Result<()> {
        self.append("event", &event_summary(event))
    }

    pub(crate) fn log_approval(
        &mut self,
        call: &ToolCall,
        decision: ApprovalDecision,
    ) -> Result<()> {
        let decision = match decision {
            ApprovalDecision::Allow => "allow",
            ApprovalDecision::Deny => "deny",
        };
        self.append(
            "approval",
            &format!("{decision} {}", crate::tool_display::summarize(call)),
        )
    }

    fn append(&mut self, kind: &str, text: &str) -> Result<()> {
        let record = TranscriptRecord {
            kind: kind.to_string(),
            text: text.to_string(),
        };
        serde_json::to_writer(&mut self.file, &record)?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        Ok(())
    }
}

pub(crate) struct TranscriptUi<'a> {
    inner: &'a mut dyn Ui,
    log: &'a mut TranscriptLog,
}

impl<'a> TranscriptUi<'a> {
    pub(crate) fn new(inner: &'a mut dyn Ui, log: &'a mut TranscriptLog) -> Self {
        Self { inner, log }
    }
}

impl Ui for TranscriptUi<'_> {
    fn next_prompt(&mut self) -> Result<Option<String>> {
        let prompt = self.inner.next_prompt()?;
        if let Some(prompt) = prompt.as_ref() {
            self.log.log_prompt(prompt.trim())?;
        }
        Ok(prompt)
    }

    fn emit(&mut self, event: UiEvent) -> Result<()> {
        self.inner.emit(event.clone())?;
        self.log.log_event(&event)
    }

    fn request_approval(&mut self, call: &ToolCall) -> Result<ApprovalDecision> {
        let decision = self.inner.request_approval(call)?;
        self.log.log_approval(call, decision)?;
        Ok(decision)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TranscriptRecord {
    pub(crate) kind: String,
    pub(crate) text: String,
}

#[cfg(test)]
fn read_records(path: &std::path::Path) -> Result<Vec<TranscriptRecord>> {
    let file = File::open(path)
        .with_context(|| format!("failed to open transcript {}", path.display()))?;
    BufReader::new(file)
        .lines()
        .map(|line| {
            let line = line?;
            serde_json::from_str(&line).context("failed to parse transcript record")
        })
        .collect()
}

fn transcript_enabled() -> bool {
    std::env::var("IRIS_TRANSCRIPT")
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn event_summary(event: &UiEvent) -> String {
    match event {
        UiEvent::SessionStarted => "session started".to_string(),
        UiEvent::AssistantText(text) => format!("assistant {}", redact_text(text)),
        UiEvent::AssistantTextDelta(delta) => {
            format!("assistant_delta {} chars", delta.chars().count())
        }
        UiEvent::AssistantTextEnd(text) if !text.is_empty() => {
            format!("assistant {}", redact_text(text))
        }
        UiEvent::AssistantTextEnd(_) => "assistant text end".to_string(),
        UiEvent::ToolProposed(call) => {
            format!("tool proposed {}", crate::tool_display::summarize(call))
        }
        UiEvent::DiffPreview { call, diff } => format!(
            "diff preview {} ({} chars)",
            crate::tool_display::summarize(call),
            diff.chars().count()
        ),
        UiEvent::ToolDenied(call) => {
            format!("tool denied {}", crate::tool_display::summarize(call))
        }
        UiEvent::ToolResult { call, content } => format!(
            "tool result {} ({} chars)",
            crate::tool_display::summarize(call),
            content.chars().count()
        ),
        UiEvent::ToolError { call, message } => format!(
            "tool error {}: {}",
            crate::tool_display::summarize(call),
            redact_text(message)
        ),
        UiEvent::Notice(message) => format!("notice {}", redact_text(message)),
        UiEvent::TurnError { kind, message } => {
            format!("turn error {kind:?}: {}", redact_text(message))
        }
        UiEvent::TurnComplete => "turn complete".to_string(),
    }
}

fn redact_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut word = String::new();

    for ch in text.chars() {
        if ch.is_whitespace() {
            push_redacted_word(&mut out, &word);
            word.clear();
            out.push(ch);
        } else {
            word.push(ch);
        }
    }
    push_redacted_word(&mut out, &word);

    out
}

fn push_redacted_word(out: &mut String, word: &str) {
    if word.is_empty() {
        return;
    }
    let lower = word.to_ascii_lowercase();
    if lower.contains("sk-")
        || lower.contains("bearer")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("api_key")
    {
        out.push_str("<redacted>");
    } else {
        out.push_str(word);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::ToolCall;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("iris-transcript-{name}-{nanos}.log"))
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "call_1".to_string(),
            name: "write".to_string(),
            arguments: json!({ "path": "note.txt", "content": "secret" }),
        }
    }

    #[test]
    fn transcript_round_trips_records() -> Result<()> {
        let path = temp_path("roundtrip");
        let mut log = TranscriptLog::open_at(&path)?;

        log.log_prompt("hello\n  sk-secret")?;
        log.log_event(&UiEvent::ToolResult {
            call: call(),
            content: "file contents are not persisted".to_string(),
        })?;
        log.log_event(&UiEvent::AssistantTextEnd(
            "model says \"sk-model-secret\"".to_string(),
        ))?;
        log.log_approval(&call(), ApprovalDecision::Deny)?;
        drop(log);

        let records = read_records(&path)?;
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].kind, "user");
        assert!(records[0].text.contains("hello\n  <redacted>"));
        assert!(!records[1].text.contains("file contents are not persisted"));
        assert!(records[2].text.contains("<redacted>"));
        assert_eq!(records[3].text, "deny write note.txt");
        fs::remove_file(path)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn transcript_file_is_owner_only() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let path = temp_path("mode");
        let _log = TranscriptLog::open_at(&path)?;
        let mode = fs::metadata(&path)?.permissions().mode() & 0o777;

        assert_eq!(mode, 0o600);
        fs::remove_file(path)?;
        Ok(())
    }
}
