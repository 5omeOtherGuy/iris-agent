//! `read_output` — page back an oversized tool output stored behind a handle.
//!
//! Retrieval half of the output-offload system (#61 / #205). When a tool result
//! exceeds the inline threshold, Nexus stores the full text in the handle store
//! and leaves a head/tail preview naming an `outputHandle` id. This tool lets the
//! model page that full stored output back into context through the *same*
//! line-window + truncation contract as `read` (`super::text::render_line_window`),
//! so a large handle is paged, not re-inlined. It depends only on the Tier-1
//! [`ToolOutputStore`] contract via `env.output_store`, never the concrete store.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::nexus::ToolOutputStore;

use super::text::render_line_window;

pub(super) const DESCRIPTION: &str = "Page an oversized tool result by its `outputHandle`. Handles may be unknown or expired. Pages use `read`'s 2,000-line/50 KiB cap and include a continuation offset.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "handle_id": { "type": "string", "minLength": 1, "description": "outputHandle from a truncated result." },
            "offset": { "type": "integer", "minimum": 1, "default": 1, "description": "First line (1-indexed)." },
            "limit": { "type": "integer", "minimum": 1, "default": 2000, "description": "Maximum lines." }
        },
        "required": ["handle_id"]
    })
}

pub(super) fn execute(
    store: Option<&dyn ToolOutputStore>,
    args: &Value,
) -> Result<super::ToolOutput> {
    let input: ReadOutputInput = Deserialize::deserialize(args)
        .context("read_output tool arguments must include handle_id")?;
    let store = store.ok_or_else(|| {
        anyhow!("no output handle store is available in this session; nothing to read back")
    })?;
    // `get` returns `None` for both an unknown/expired id and a malformed one
    // (path-traversal ids are rejected inside the store), so a bad handle is a
    // clear tool error here, never a panic or a silent empty result.
    let content = store
        .get(&input.handle_id)?
        .ok_or_else(|| anyhow!("unknown or expired output handle: {}", input.handle_id))?;
    Ok(render_line_window(&content, input.offset, input.limit)?.into_output())
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
    use crate::handles::HandleStore;
    use crate::tools::test_support::{TestDir, temp_dir};

    /// A store seeded with `body`, returning the temp-dir guard (so it is cleaned
    /// on drop), the store, and the handle id `body` was stored under. Exercises
    /// the same content-addressed `HandleStore` the harness uses, so path-safety
    /// and id validation are the real ones.
    fn store_with(body: &str) -> (TestDir, HandleStore, String) {
        let dir = temp_dir();
        let store = HandleStore::with_dir(dir.path.join("outputs"));
        let id = store.put(body).unwrap();
        (dir, store, id)
    }

    #[test]
    fn schema_encodes_paging_defaults_and_expiry_recovery() {
        let schema = parameters();
        assert_eq!(schema["properties"]["handle_id"]["minLength"], 1);
        assert_eq!(schema["properties"]["offset"]["minimum"], 1);
        assert_eq!(schema["properties"]["offset"]["default"], 1);
        assert_eq!(schema["properties"]["limit"]["minimum"], 1);
        assert_eq!(schema["properties"]["limit"]["default"], 2_000);
        assert!(DESCRIPTION.contains("expired"));
    }

    #[test]
    fn missing_store_is_a_tool_error() {
        let err = execute(None, &json!({ "handle_id": "deadbeef" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no output handle store"), "{err}");
    }

    #[test]
    fn unknown_handle_id_is_a_tool_error() {
        let (_dir, store, _id) = store_with("stored body\nsecond line\n");
        let err = execute(Some(&store), &json!({ "handle_id": "deadbeef" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown or expired output handle"), "{err}");
    }

    #[test]
    fn malformed_handle_id_is_rejected_not_traversed() {
        let (_dir, store, _id) = store_with("stored body\n");
        // A forged id with traversal characters must not escape the store dir;
        // the store returns `None`, so the tool reports an unknown handle.
        let err = execute(Some(&store), &json!({ "handle_id": "../secret" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown or expired output handle"), "{err}");
    }

    #[test]
    fn returns_full_stored_output_by_handle() {
        let (_dir, store, id) = store_with("alpha\nbeta\ngamma\n");
        let out = execute(Some(&store), &json!({ "handle_id": id })).unwrap();
        assert!(out.content.contains("\u{2192}alpha"));
        assert!(out.content.contains("3\u{2192}gamma"));
        assert_eq!(out.metadata.get("total_lines"), Some(&json!(3)));
        assert_eq!(out.metadata.get("truncated"), Some(&json!(false)));
    }

    #[test]
    fn offset_and_limit_window_the_handle() {
        let body: String = (1..=10).map(|n| format!("line{n}\n")).collect();
        let (_dir, store, id) = store_with(&body);
        let out = execute(
            Some(&store),
            &json!({ "handle_id": id, "offset": 3, "limit": 2 }),
        )
        .unwrap();
        assert!(out.content.contains("3\u{2192}line3"));
        assert!(out.content.contains("4\u{2192}line4"));
        assert!(!out.content.contains("line5"));
        assert!(out.content.contains("Use offset=5 to continue"));
        assert_eq!(out.metadata.get("truncated"), Some(&json!(true)));
    }

    #[test]
    fn limit_emits_read_style_truncation_notice() {
        let body: String = (1..=100).map(|n| format!("line{n}\n")).collect();
        let (_dir, store, id) = store_with(&body);
        let out = execute(Some(&store), &json!({ "handle_id": id, "limit": 5 })).unwrap();
        assert!(out.content.contains("more lines in file"));
        assert!(out.content.contains("Use offset=6 to continue"));
    }
}
