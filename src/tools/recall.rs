//! `recall` -- retrieve the original turns of a compacted range on demand
//! (ADR-0046, issue #373).
//!
//! Compaction (ADR-0009) replaces a covered range of turns with a short summary.
//! The originals stay durable in the JSONL transcript, but the running agent
//! only sees the summary, so a detail the summary dropped is unreachable
//! mid-session. This tool makes each compacted range retrievable: at compaction
//! the harness serializes the covered turns into a blob stored behind a
//! session-scoped handle (the SAME `ToolOutputStore` / ADR-0011 discipline the
//! oversized-output offload uses), and the rebuilt summary carries a recall
//! reference naming that handle. The model reads the reference, calls `recall`
//! with the handle, and gets the original turns back -- windowed, bounded, and
//! with tool-call/tool-result pairs kept intact.
//!
//! Read-only over this session's own transcript: it takes no workspace path and
//! no shell surface, needs no approval, and reads only through the handle store
//! (a forged/malformed handle is rejected by the store, never traversed). A
//! recall result that is still oversized after windowing offloads behind a fresh
//! handle through the standard Nexus tool-result path, exactly like any other
//! large tool output (ADR-0011); the model then narrows with `pattern`/`offset`.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::nexus::{Message, Role, ToolOutputStore};

/// Model-facing tool name (also the key the system-prompt fragment and the
/// registry gate on). Kept in one place so the tool, its fragment, and the
/// compaction marker never drift.
pub(crate) const RECALL_TOOL_NAME: &str = "recall";

/// Default number of turn-groups a windowed (non-search) recall returns when
/// the call gives no explicit `limit`.
const DEFAULT_LIMIT: usize = 20;
/// Hard cap on turn-groups per windowed recall, so a single call cannot page the
/// whole covered range back into context at once (it offloads/pages instead).
const MAX_LIMIT: usize = 100;
/// Hard cap on the number of matches a search-mode recall returns, so a broad
/// pattern stays bounded; the model narrows and does a windowed read on a hit.
const MAX_SEARCH_HITS: usize = 30;
/// Per-turn content shown in a search hit preview (search returns locations, not
/// full turns; a windowed read then retrieves the hit verbatim).
const SEARCH_PREVIEW_CHARS: usize = 200;

pub(super) const DESCRIPTION: &str = "Retrieve the ORIGINAL turns of a compacted range that a summary replaced (ADR-0046). After compaction the context shows a summary plus a recall reference naming a `handle`; pass that `handle` to page the original turns back, windowed with `offset` (1-indexed turn-group) and `limit`. Narrow to an entry-id `from`..`to` span, or pass a `pattern` to search the range and get back matching turns with their entry ids (then do a windowed read on a hit). Tool-call/tool-result pairs are returned intact. Read-only over this session's own transcript: no file path, no shell, no approval.";

pub(super) fn parameters() -> Value {
    json!({
        "type": "object",
        "properties": {
            "handle": { "type": "string", "description": "The recall handle id from a compaction reference (the `recall(handle=...)` marker in the summary)." },
            "from": { "type": "string", "description": "Optional inclusive start entry id: narrow the returned turns to the [from, to] span within the compacted range." },
            "to": { "type": "string", "description": "Optional inclusive end entry id for the span (used with `from`)." },
            "pattern": { "type": "string", "description": "Optional search: return only turns whose content contains this substring, with their entry ids (bounded count)." },
            "offset": { "type": "integer", "description": "1-indexed turn-group to start the window at (windowed reads only)." },
            "limit": { "type": "integer", "description": "Maximum turn-groups to return in a windowed read." }
        },
        "required": ["handle"]
    })
}

/// One covered turn as stored in the recall blob: its durable entry id (`None`
/// for a summary position or a legacy id-less entry), role, verbatim content,
/// and -- for tool-call/result turns -- the pairing fields so a rebuilt pair is
/// recognizable and never split across a window boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecalledTurn {
    pub(crate) id: Option<String>,
    pub(crate) role: String,
    pub(crate) content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) tool_call_id: Option<String>,
}

/// The serialized covered range stored behind a recall handle: the inclusive
/// entry-id bounds and the covered turns in order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RecallBlob {
    pub(crate) covered_from: String,
    pub(crate) covered_to: String,
    pub(crate) turns: Vec<RecalledTurn>,
}

/// Serialize a covered range into the recall blob the handle stores. `entry_ids`
/// is parallel to `messages` (the compaction's covered slice and its ids), so
/// each turn keeps the durable id the read path threaded (including across a
/// startup resume, #377).
pub(crate) fn serialize_covered(
    messages: &[Message],
    entry_ids: &[Option<String>],
    from: &str,
    to: &str,
) -> String {
    let turns = messages
        .iter()
        .enumerate()
        .map(|(i, message)| RecalledTurn {
            id: entry_ids.get(i).cloned().flatten(),
            role: message.role.as_str().to_string(),
            content: message.content.clone(),
            tool_name: message.tool_name.clone(),
            tool_call_id: message.tool_call_id.clone(),
        })
        .collect();
    let blob = RecallBlob {
        covered_from: from.to_string(),
        covered_to: to.to_string(),
        turns,
    };
    // Serialization is infallible for these owned scalar fields; fall back to an
    // empty object rather than panicking inside the compaction path.
    serde_json::to_string(&blob).unwrap_or_else(|_| "{}".to_string())
}

/// The recall reference embedded in a rebuilt summary (ADR-0046 / ADR-0045
/// needle). It must survive rebuild verbatim: it is the only anchor telling the
/// model the covered originals are retrievable and under which handle.
pub(crate) fn recall_marker(handle: &str, from: &str, to: &str) -> String {
    format!(
        "[recall] The original turns of this compacted range (entry ids {from}..{to}) are \
         retrievable with recall(handle=\"{handle}\")."
    )
}

#[derive(Debug, Deserialize)]
struct RecallInput {
    handle: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    pattern: Option<String>,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

/// Test-only re-export so harness-tier integration tests (`wayland`) can drive
/// the tool body directly through the same `ToolOutputStore` contract the
/// registry uses, without widening the production surface.
#[cfg(test)]
pub(crate) fn execute_for_test(
    store: Option<&dyn ToolOutputStore>,
    args: &Value,
) -> Result<super::ToolOutput> {
    execute(store, args)
}

pub(super) fn execute(
    store: Option<&dyn ToolOutputStore>,
    args: &Value,
) -> Result<super::ToolOutput> {
    let input: RecallInput = serde_json::from_value(args.clone())
        .context("recall tool arguments must include a string `handle`")?;
    let store = store.ok_or_else(|| {
        anyhow!("no session handle store is available; there is nothing to recall")
    })?;
    // `get` returns `None` for an unknown, expired, or malformed (path-traversal)
    // id -- the store validates ids before any read -- so a bad handle is a clean
    // tool error here, never a panic or a traversal.
    let content = store
        .get(&input.handle)?
        .ok_or_else(|| anyhow!("unknown or expired recall handle: {}", input.handle))?;
    let blob: RecallBlob = serde_json::from_str(&content).map_err(|_| {
        anyhow!(
            "handle {} is not a recall handle (it holds a different kind of stored output)",
            input.handle
        )
    })?;

    // Optional entry-id span, validated at the boundary: a non-hex bound is
    // malformed input and rejected rather than silently ignored.
    let span = parse_span(input.from.as_deref(), input.to.as_deref())?;
    let selected: Vec<&RecalledTurn> = match span {
        Some((lo, hi)) => blob
            .turns
            .iter()
            .filter(|turn| turn_in_span(turn, lo, hi))
            .collect(),
        None => blob.turns.iter().collect(),
    };

    if let Some(pattern) = input.pattern.as_deref().filter(|p| !p.is_empty()) {
        return Ok(search(&selected, pattern, &blob));
    }
    Ok(window(&selected, input.offset, input.limit, &blob))
}

/// Parse the optional `from`/`to` entry-id span into an inclusive `(lo, hi)`
/// numeric range. Entry ids are hex of the session's monotonic seq counter, so
/// a numeric compare is the range test. `None` when neither bound is given; an
/// error when a bound is non-hex or the range runs backwards.
fn parse_span(from: Option<&str>, to: Option<&str>) -> Result<Option<(u64, u64)>> {
    let parse = |label: &str, value: &str| -> Result<u64> {
        u64::from_str_radix(value.trim(), 16)
            .map_err(|_| anyhow!("recall `{label}` must be a hex entry id, got {value:?}"))
    };
    match (from, to) {
        (None, None) => Ok(None),
        (from, to) => {
            // A one-sided bound is open on the missing side.
            let lo = from.map(|v| parse("from", v)).transpose()?.unwrap_or(0);
            let hi = to.map(|v| parse("to", v)).transpose()?.unwrap_or(u64::MAX);
            if lo > hi {
                return Err(anyhow!(
                    "recall span runs backwards: from {lo:#x} > to {hi:#x}"
                ));
            }
            Ok(Some((lo, hi)))
        }
    }
}

/// Whether a turn's entry id falls inside `[lo, hi]`. A turn with no id (a
/// legacy id-less entry) is excluded from an explicit span -- the span is an
/// id-addressed request.
fn turn_in_span(turn: &RecalledTurn, lo: u64, hi: u64) -> bool {
    turn.id
        .as_deref()
        .and_then(|id| u64::from_str_radix(id, 16).ok())
        .is_some_and(|n| n >= lo && n <= hi)
}

/// Group turns so a tool-call/tool-result pair is never split across a window
/// boundary: a `tool`-role result attaches to the group of the preceding turn
/// (its tool call), and consecutive results attach too. A leading result with no
/// predecessor starts its own group.
fn group_units(turns: &[&RecalledTurn]) -> Vec<Vec<usize>> {
    let mut units: Vec<Vec<usize>> = Vec::new();
    for (i, turn) in turns.iter().enumerate() {
        if turn.role == Role::Tool.as_str() && !units.is_empty() {
            units.last_mut().expect("non-empty checked").push(i);
        } else {
            units.push(vec![i]);
        }
    }
    units
}

/// Windowed read: return whole turn-groups from `offset` (1-indexed) up to
/// `limit`, bounded by [`MAX_LIMIT`]. Emits a continuation notice and
/// `truncated`/`next_offset` metadata when groups remain, mirroring `read`.
fn window(
    turns: &[&RecalledTurn],
    offset: Option<i64>,
    limit: Option<i64>,
    blob: &RecallBlob,
) -> super::ToolOutput {
    let units = group_units(turns);
    let total_units = units.len();
    let start = offset
        .filter(|o| *o > 0)
        .map(|o| o as usize - 1)
        .unwrap_or(0);
    let limit = limit
        .filter(|l| *l > 0)
        .map(|l| (l as usize).min(MAX_LIMIT))
        .unwrap_or(DEFAULT_LIMIT);

    if start >= total_units {
        return super::ToolOutput::text(format!(
            "recall: no turns at offset {} (the range has {total_units} turn-group(s)).",
            start + 1
        ))
        .with("total_turn_groups", json!(total_units))
        .with("truncated", json!(false));
    }

    let end = (start + limit).min(total_units);
    let mut body = String::new();
    for (unit_no, unit) in units[start..end].iter().enumerate() {
        for &ti in unit {
            body.push_str(&render_turn(start + unit_no + 1, turns[ti]));
            body.push('\n');
        }
    }
    let truncated = end < total_units;
    if truncated {
        body.push_str(&format!(
            "\n... {} more turn-group(s) in this range. Use offset={} to continue, or pattern= to search.\n",
            total_units - end,
            end + 1
        ));
    }
    super::ToolOutput::text(body.trim_end().to_string())
        .with("covered_from", json!(blob.covered_from))
        .with("covered_to", json!(blob.covered_to))
        .with("total_turn_groups", json!(total_units))
        .with("returned_turn_groups", json!(end - start))
        .with("truncated", json!(truncated))
        .with(
            "next_offset",
            json!(if truncated { Some(end + 1) } else { None }),
        )
}

/// Search mode: return the entry ids and previews of turns whose content
/// contains `pattern`, bounded by [`MAX_SEARCH_HITS`]. The model then does a
/// windowed read targeting a hit instead of paging the whole range.
fn search(turns: &[&RecalledTurn], pattern: &str, blob: &RecallBlob) -> super::ToolOutput {
    let hits: Vec<&&RecalledTurn> = turns
        .iter()
        .filter(|turn| turn.content.contains(pattern))
        .collect();
    let total_hits = hits.len();
    if total_hits == 0 {
        return super::ToolOutput::text(format!("recall search: no turns match {pattern:?}."))
            .with("match_count", json!(0));
    }
    let shown = total_hits.min(MAX_SEARCH_HITS);
    let mut body = format!("recall search for {pattern:?}: {total_hits} match(es).\n");
    for turn in hits.iter().take(shown) {
        let id = turn.id.as_deref().unwrap_or("(none)");
        body.push_str(&format!(
            "- id={id} [{}] {}\n",
            turn.role,
            preview(&turn.content)
        ));
    }
    if total_hits > shown {
        body.push_str(&format!(
            "... {} more match(es); refine the pattern. Then recall(handle, from=<id>, to=<id>) to read a hit.\n",
            total_hits - shown
        ));
    }
    super::ToolOutput::text(body.trim_end().to_string())
        .with("covered_from", json!(blob.covered_from))
        .with("covered_to", json!(blob.covered_to))
        .with("match_count", json!(total_hits))
        .with("returned_matches", json!(shown))
}

/// A single-line, length-bounded preview of a turn's content for a search hit.
fn preview(content: &str) -> String {
    let flat: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= SEARCH_PREVIEW_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(SEARCH_PREVIEW_CHARS).collect();
    format!("{cut}...")
}

/// Render one turn verbatim with a header naming its position, entry id, role,
/// and (for tool turns) the tool name, so a rebuilt pair is legible.
fn render_turn(position: usize, turn: &RecalledTurn) -> String {
    let id = turn.id.as_deref().unwrap_or("(none)");
    let mut header = format!("#{position} id={id} [{}]", turn.role);
    if let Some(name) = &turn.tool_name {
        header.push_str(&format!(" tool={name}"));
    }
    format!("{header}\n{}", turn.content)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handles::HandleStore;
    use crate::nexus::{Message, ToolCall};
    use crate::tools::test_support::{TestDir, temp_dir};

    /// A store seeded with a serialized covered range; returns the temp-dir
    /// guard, the store, and the recall handle id the blob was stored under.
    fn store_with(messages: &[Message], ids: &[Option<String>]) -> (TestDir, HandleStore, String) {
        let dir = temp_dir();
        let store = HandleStore::with_dir(dir.path.join("outputs"));
        let from = ids.first().cloned().flatten().unwrap_or_default();
        let to = ids.last().cloned().flatten().unwrap_or_default();
        let blob = serialize_covered(messages, ids, &from, &to);
        let id = store.put(&blob).unwrap();
        (dir, store, id)
    }

    fn tool_call_msg(call_id: &str, name: &str, args: &str) -> Message {
        Message::assistant_tool_call(&ToolCall {
            id: call_id.to_string(),
            name: name.to_string(),
            arguments: serde_json::json!({ "raw": args }),
            thought_signature: None,
        })
    }

    #[test]
    fn recall_returns_original_turns_with_pairs_intact() {
        // A user turn, an assistant tool call, and its tool result: recall must
        // return all three and keep the call+result adjacent (one turn-group).
        let messages = [
            Message::user("please read the config"),
            tool_call_msg("call_1", "read", "config.toml"),
            Message::tool_result("call_1", "read", "PORT=8080"),
            Message::assistant("done"),
        ];
        let ids = [
            Some("01".to_string()),
            Some("02".to_string()),
            Some("03".to_string()),
            Some("04".to_string()),
        ];
        let (_dir, store, handle) = store_with(&messages, &ids);

        let out = execute(Some(&store), &json!({ "handle": handle })).unwrap();
        // All original content is present.
        assert!(
            out.content.contains("please read the config"),
            "{}",
            out.content
        );
        assert!(out.content.contains("PORT=8080"), "{}", out.content);
        assert!(out.content.contains("done"), "{}", out.content);
        // Entry ids are surfaced.
        assert!(out.content.contains("id=01"));
        assert!(out.content.contains("id=03"));
        // The call and its result share one turn-group (#2), so the result is
        // NOT numbered as its own group -- the pair is not split.
        let call_pos = out.content.find("id=02").unwrap();
        let result_pos = out.content.find("id=03").unwrap();
        assert!(call_pos < result_pos);
        assert!(
            out.content[call_pos..result_pos].contains("#2"),
            "call is group #2"
        );
        // The result line is part of group #2, not a new #3 group header.
        assert!(
            !out.content[result_pos..].starts_with("#3 id=03"),
            "tool result must not open its own turn-group"
        );
    }

    #[test]
    fn search_returns_matching_turns_with_entry_ids_bounded() {
        let messages: Vec<Message> = (0..40)
            .map(|n| Message::user(&format!("turn {n} NEEDLE marker")))
            .collect();
        let ids: Vec<Option<String>> = (0..40).map(|n| Some(format!("{n:02x}"))).collect();
        let (_dir, store, handle) = store_with(&messages, &ids);

        let out = execute(
            Some(&store),
            &json!({ "handle": handle, "pattern": "NEEDLE" }),
        )
        .unwrap();
        // Every turn matches, but the hit list is bounded.
        assert_eq!(out.metadata.get("match_count"), Some(&json!(40)));
        assert_eq!(
            out.metadata.get("returned_matches"),
            Some(&json!(MAX_SEARCH_HITS))
        );
        // Hits carry entry ids so a follow-up windowed read can target one.
        assert!(out.content.contains("id=00"));
        assert!(out.content.contains("more match(es)"));
    }

    #[test]
    fn span_narrows_to_the_entry_id_range() {
        let messages: Vec<Message> = (0..10)
            .map(|n| Message::user(&format!("turn {n}")))
            .collect();
        let ids: Vec<Option<String>> = (0..10).map(|n| Some(format!("{n:02x}"))).collect();
        let (_dir, store, handle) = store_with(&messages, &ids);

        // Entry ids 03..05 inclusive.
        let out = execute(
            Some(&store),
            &json!({ "handle": handle, "from": "03", "to": "05" }),
        )
        .unwrap();
        assert!(out.content.contains("turn 3"));
        assert!(out.content.contains("turn 5"));
        assert!(!out.content.contains("turn 2"));
        assert!(!out.content.contains("turn 6"));
    }

    #[test]
    fn window_offset_and_limit_page_turn_groups() {
        let messages: Vec<Message> = (0..10)
            .map(|n| Message::user(&format!("turn {n}")))
            .collect();
        let ids: Vec<Option<String>> = (0..10).map(|n| Some(format!("{n:02x}"))).collect();
        let (_dir, store, handle) = store_with(&messages, &ids);

        let out = execute(
            Some(&store),
            &json!({ "handle": handle, "offset": 3, "limit": 2 }),
        )
        .unwrap();
        assert!(out.content.contains("turn 2")); // group #3 (1-indexed)
        assert!(out.content.contains("turn 3"));
        assert!(!out.content.contains("turn 4"));
        assert_eq!(out.metadata.get("truncated"), Some(&json!(true)));
        assert_eq!(out.metadata.get("next_offset"), Some(&json!(5)));
    }

    #[test]
    fn unknown_or_malformed_handle_is_a_tool_error_not_a_traversal() {
        let (_dir, store, _handle) = store_with(&[Message::user("x")], &[Some("01".to_string())]);
        let unknown = execute(Some(&store), &json!({ "handle": "deadbeef" }))
            .unwrap_err()
            .to_string();
        assert!(
            unknown.contains("unknown or expired recall handle"),
            "{unknown}"
        );
        // A forged traversal id is rejected by the store, surfaced as unknown.
        let traversal = execute(Some(&store), &json!({ "handle": "../secret" }))
            .unwrap_err()
            .to_string();
        assert!(
            traversal.contains("unknown or expired recall handle"),
            "{traversal}"
        );
    }

    #[test]
    fn missing_store_is_a_tool_error() {
        let err = execute(None, &json!({ "handle": "deadbeef" }))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no session handle store"), "{err}");
    }

    #[test]
    fn malformed_span_bound_is_rejected() {
        let (_dir, store, handle) = store_with(&[Message::user("x")], &[Some("01".to_string())]);
        let err = execute(
            Some(&store),
            &json!({ "handle": handle, "from": "not-hex", "to": "05" }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("must be a hex entry id"), "{err}");
    }

    #[test]
    fn parameters_expose_no_path_or_shell_surface() {
        let params = parameters();
        let props = params["properties"].as_object().unwrap();
        for forbidden in ["path", "file_path", "command", "cwd", "shell", "workspace"] {
            assert!(
                !props.contains_key(forbidden),
                "recall must not expose a {forbidden} argument"
            );
        }
        // Only the read-only addressing/windowing/search args exist.
        let mut keys: Vec<&String> = props.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["from", "handle", "limit", "offset", "pattern", "to"]
        );
    }
}
