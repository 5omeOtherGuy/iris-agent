//! `read_output` — dereference an offloaded tool output by handle (issue #205).
//!
//! Oversized tool outputs are stored out of provider context behind a
//! content-addressed handle with only a head/tail preview inline (issue #61).
//! This tool is the retrieval half: the model pages through the stored output
//! with the same line-window + truncation contract as `read`, so a large handle
//! is paged, never re-inlined wholesale. Window output is capped at the same
//! 50KB the offload threshold uses, and the normal offload policy still applies
//! to this tool's results, so dereferencing can never re-inline an oversized
//! payload.

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::nexus::ToolOutputStore;

use super::read::window_content;

pub(super) const DESCRIPTION: &str = "Read back the full output of an earlier tool call that was stored out of context behind an output handle (see the 'retrieve via the read_output tool' notice and the outputHandle metadata). Output is line-numbered and truncated to 2000 lines or 50KB (whichever is hit first); use offset/limit to page through the stored output.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "handle_id": { "type": "string", "description": "The output handle id to read (from the outputHandle metadata or the elision notice)" },
            "offset": { "type": "integer", "description": "Line number to start reading from (1-indexed)" },
            "limit": { "type": "integer", "description": "Maximum number of lines to read" }
        },
        "required": ["handle_id"]
    })
}

pub(super) fn execute(
    store: Option<&dyn ToolOutputStore>,
    args: &Value,
) -> Result<super::ToolOutput> {
    let input: ReadOutputInput = serde_json::from_value(args.clone())
        .context("read_output tool arguments must include handle_id")?;
    let Some(store) = store else {
        bail!(
            "this session has no output store attached, so no offloaded outputs exist to read \
             back"
        );
    };
    let Some(content) = store.get(&input.handle_id)? else {
        bail!(
            "unknown output handle: {}. Handle ids come from the outputHandle metadata of an \
             earlier oversized tool result in this session.",
            input.handle_id
        );
    };
    let window = window_content(&content, input.offset, input.limit)?;
    Ok(super::ToolOutput::text(window.text)
        .with("bytes", json!(content.len()))
        .with("lines", json!(window.lines))
        .with("total_lines", json!(window.total_lines))
        .with("truncated", json!(window.truncated)))
}

#[derive(Debug, Deserialize)]
struct ReadOutputInput {
    handle_id: String,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// In-memory store: `put` is content-keyed like the real store, `get` looks
    /// the id up.
    struct MemStore {
        entries: RefCell<HashMap<String, String>>,
    }

    impl MemStore {
        fn with(id: &str, content: &str) -> Self {
            let mut entries = HashMap::new();
            entries.insert(id.to_string(), content.to_string());
            Self {
                entries: RefCell::new(entries),
            }
        }
    }

    impl ToolOutputStore for MemStore {
        fn put(&self, content: &str) -> Result<String> {
            let id = format!("id{}", self.entries.borrow().len());
            self.entries
                .borrow_mut()
                .insert(id.clone(), content.to_string());
            Ok(id)
        }
        fn get(&self, id: &str) -> Result<Option<String>> {
            Ok(self.entries.borrow().get(id).cloned())
        }
    }

    fn run(store: &MemStore, args: Value) -> Result<super::super::ToolOutput> {
        execute(Some(store), &args)
    }

    #[test]
    fn read_output_pages_a_stored_output_with_line_numbers() {
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        let store = MemStore::with("abc123", &body);

        let out = run(
            &store,
            json!({ "handle_id": "abc123", "offset": 3, "limit": 2 }),
        )
        .unwrap();

        assert!(out.content.contains("3\u{2192}line3"));
        assert!(out.content.contains("4\u{2192}line4"));
        assert!(!out.content.contains("line5"));
        assert!(out.content.contains("Use offset=5 to continue"));
        assert_eq!(out.metadata.get("total_lines"), Some(&json!(10)));
        assert_eq!(out.metadata.get("truncated"), Some(&json!(true)));
    }

    #[test]
    fn read_output_window_stays_within_the_offload_threshold() {
        // A stored output far over the 50KB threshold: one page of it must come
        // back capped at the read contract's 50KB, so a dereference result is
        // never itself an oversized payload.
        let body = "filler line for a very large stored tool output\n".repeat(4000);
        assert!(body.len() > 100 * 1024);
        let store = MemStore::with("beef", &body);

        let out = run(&store, json!({ "handle_id": "beef" })).unwrap();

        assert!(
            out.content.len() <= 50 * 1024 + 256,
            "window must stay near the 50KB cap, got {}",
            out.content.len()
        );
        assert!(out.content.contains("50KB limit"), "{}", &out.content[out.content.len() - 200..]);
    }

    #[test]
    fn read_output_unknown_handle_is_an_actionable_error() {
        let store = MemStore::with("abc123", "content");
        let err = run(&store, json!({ "handle_id": "nope" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown output handle: nope"), "{err}");
    }

    #[test]
    fn read_output_without_a_store_explains_why() {
        let err = execute(None, &json!({ "handle_id": "abc" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no output store"), "{err}");
    }

    #[test]
    fn read_output_requires_handle_id() {
        let store = MemStore::with("abc123", "content");
        let err = run(&store, json!({})).unwrap_err().to_string();
        assert!(err.contains("handle_id"), "{err}");
    }
}
