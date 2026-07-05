//! Tier-2 harness-owned system prompt assembly: a fragment/slot "baukasten".
//!
//! The provider-visible instruction string is composed from the shipped
//! in-binary fragments ([`defaults::DEFAULTS`], the single source of truth per
//! ADR-0026) plus auto-injected dynamic context. Each fragment is one XML
//! block; its `name` is the tag and its body is the inner text. Fragments are
//! never loaded from disk: ADR-0026 removed the user (`~/.iris/fragments`) and
//! repo (`<workspace>/.iris/fragments`) file loading -- and with it the
//! system-prompt-injection surface and the fragment-trust gate. User and
//! project steering happens through `AGENTS.md`/`CLAUDE.md`, which are still
//! folded in as `<project_context>`.
//!
//! Fragments remain the internal assembly abstraction: the selector schema
//! (ADR-0013) and named slots (ADR-0015) still order and conditionally include
//! them; only their provenance is in-binary.
//!
//! Mirrors pi's `harness/system-prompt.ts` (assembly owned by the harness, not
//! the terminal UI) and `core/resource-loader.ts` (project-doc discovery walk)
//! for the dynamic context. Intentionally NOT adopted: Codex's per-turn context
//! diff-injection and `TurnContextItem` persistence (this builds a full prompt),
//! Codex's multi-root `<environment_context>` arrays (single cwd), pi's
//! prompt-templates, and any prompt-caching.
//!
//! ## Order
//!
//! 1. `identity` (anchored first),
//! 2. middle fragments: slotted by ascending `slot` (same slot: alphabetical by
//!    `name`), then unslotted fragments alphabetically,
//! 3. dynamic context: `<project_context>` (AGENTS.md/CLAUDE.md), then the
//!    skills seam (deferred -- no skill registry yet), then the `Current date` /
//!    `Current working directory` lines,
//! 4. the anchored tool tail: `available_tools` (generated), then
//!    `available_tool_guidelines` (generated), then `tool_use` (authored).
//!
//! Any fragment whose body is empty/whitespace emits nothing (no tag). `slot`
//! is a sort key, not a uniqueness constraint: two fragments may share a slot.
//! `slot: 0` is the off switch: a fragment opts out entirely by setting it, so
//! it is dropped before anchoring/ordering -- anchors included.
//!
//! ## Purity
//!
//! [`assemble`] is read-only (it never writes), and the core [`build_prompt`] is
//! a pure function of its inputs, so per-turn re-assembly (deferred) is a later
//! no-restructure change: call it again with fresh dynamic context.
//!
//! ## Path safety
//!
//! Project-doc discovery walks cwd -> filesystem root reading `AGENTS.md` /
//! `CLAUDE.md` like pi/Codex. Ancestor docs are treated as user-owned trusted
//! config (the same trust class as `~/.iris/settings.json`, which the harness
//! already reads from `HOME`): a normal cloned repo only controls files inside
//! the workspace, not ancestor directories. Every file folded into the prompt
//! (the project docs) is read through [`read_regular_bounded`], which refuses
//! symlinks (via `symlink_metadata`), opens the final component with
//! `O_NOFOLLOW` to close the check/open race, and caps the bytes read. So a
//! cloned repo cannot plant `AGENTS.md -> ~/.ssh/id_rsa` to exfiltrate host
//! files into the prompt.

mod defaults;

use std::collections::HashSet;
use std::io::Read;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::nexus::Tools;

use defaults::DEFAULTS;

/// Anchored fragment name pinned first.
const ANCHOR_IDENTITY: &str = "identity";
/// Anchored fragment name pinned last (authored tool guidance).
const ANCHOR_TOOL_USE: &str = "tool_use";
/// Generated tool-tail block: the live tool list.
const GEN_AVAILABLE_TOOLS: &str = "available_tools";
/// Generated tool-tail block: the tool guidelines.
const GEN_TOOL_GUIDELINES: &str = "available_tool_guidelines";
/// Tool-gated fragment: the recall guidance (ADR-0046) documents the compaction
/// markers and when to recall, and ships only when the recall tool is actually
/// registered -- so a build without the tool never advertises an absent
/// affordance.
const FRAGMENT_COMPACTION_RECALL: &str = "compaction_recall";

/// Project-doc filenames discovered per directory, in priority order (first
/// existing wins for that directory). Mirrors pi's candidate list.
const DOC_CANDIDATES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Upper bound on bytes folded into the prompt per discovered project doc, so a
/// runaway or hostile file cannot balloon every request / OOM the process.
const MAX_DOC_BYTES: u64 = 32 * 1024;

/// One fragment: `name` (the xml tag), an optional `slot` sort key (`Some(0)`
/// disables the fragment), and the body (surrounding whitespace trimmed).
#[derive(Debug, Clone)]
struct Fragment {
    name: String,
    slot: Option<u32>,
    body: String,
}

/// Assemble the full provider-visible system prompt for `workspace` from the
/// in-binary shipped fragments plus dynamic context (project docs, cwd, date).
///
/// Read-only: no fragment file is discovered, materialized, or loaded from disk
/// (ADR-0026); the only filesystem access is the bounded project-doc discovery.
/// Both fresh and resumed sessions call this with the same workspace, so they
/// assemble identical instructions.
pub(crate) fn assemble(workspace: &Path, tools: &Tools) -> String {
    let docs = discover_project_docs(workspace);
    build_prompt(default_fragments(), tools, workspace, &docs, &today_ymd())
}

/// Test-only: assemble from the shipped defaults with no project docs or disk
/// access -- a hermetic instruction string for provider request-shaping tests.
#[cfg(test)]
pub(crate) fn assemble_defaults(workspace: &Path, tools: &Tools) -> String {
    build_prompt(default_fragments(), tools, workspace, &[], &today_ymd())
}

/// Pure core: build the prompt from an explicit fragment set, the live tool
/// registry, the discovered project docs, and a date string. No IO, no clock --
/// so the ordering/anchor/empty-body rules are testable in isolation and a
/// later per-turn caller can re-run it with fresh context.
fn build_prompt(
    mut fragments: Vec<Fragment>,
    tools: &Tools,
    workspace: &Path,
    docs: &[(String, String)],
    date: &str,
) -> String {
    // slot 0 means "not active at all": drop opted-out fragments up front so the
    // rule applies uniformly to anchors and middles alike.
    fragments.retain(|f| f.slot != Some(0));
    // Anchored authored blocks are consumed by name; the generated tool blocks
    // are never authored, so a stray fragment carrying their name is dropped.
    let identity = take_anchor(&mut fragments, ANCHOR_IDENTITY);
    let tool_use = take_anchor(&mut fragments, ANCHOR_TOOL_USE);
    fragments.retain(|f| f.name != GEN_AVAILABLE_TOOLS && f.name != GEN_TOOL_GUIDELINES);
    // Tool-gated fragments render only when their tool is in the live registry
    // (ADR-0046): drop the recall guidance when recall is not registered.
    if tools
        .by_name(crate::tools::recall::RECALL_TOOL_NAME)
        .is_none()
    {
        fragments.retain(|f| f.name != FRAGMENT_COMPACTION_RECALL);
    }
    let middles = order_middles(fragments);

    let mut blocks: Vec<String> = Vec::new();
    push_block(&mut blocks, ANCHOR_IDENTITY, identity.as_deref());
    for fragment in &middles {
        push_block(&mut blocks, &fragment.name, Some(&fragment.body));
    }

    // Dynamic context, pi order: project_context -> skills -> date/cwd.
    if let Some(project_context) = project_context_block(docs) {
        blocks.push(project_context);
    }
    // Skills seam (DEFERRED): Iris has no skill registry yet, so no skills block
    // is emitted. When one exists, format it here -- between project_context and
    // the date/cwd lines -- so the dynamic-context order matches pi.
    blocks.push(runtime_context_block(workspace, date));

    // Anchored tool tail: generated list, generated guidelines, authored prose.
    push_block(
        &mut blocks,
        GEN_AVAILABLE_TOOLS,
        Some(&available_tools_body(tools)),
    );
    push_block(
        &mut blocks,
        GEN_TOOL_GUIDELINES,
        Some(&tool_guidelines_body(tools)),
    );
    push_block(&mut blocks, ANCHOR_TOOL_USE, tool_use.as_deref());

    blocks.join("\n\n")
}

/// Append `<name>\n{body}\n</name>` to `blocks` when the body is non-empty after
/// trimming; an empty/whitespace body (or absent anchor) emits nothing.
fn push_block(blocks: &mut Vec<String>, name: &str, body: Option<&str>) {
    let Some(body) = body else { return };
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return;
    }
    blocks.push(format!("<{name}>\n{trimmed}\n</{name}>"));
}

/// Remove every fragment named `name` and return the winning body (the last
/// occurrence wins); `None` when no such fragment exists. Removing all matches
/// keeps a stray duplicate out of the middles.
fn take_anchor(fragments: &mut Vec<Fragment>, name: &str) -> Option<String> {
    let body = fragments
        .iter()
        .rev()
        .find(|fragment| fragment.name == name)
        .map(|fragment| fragment.body.clone());
    fragments.retain(|fragment| fragment.name != name);
    body
}

/// Order the middle fragments: slotted by ascending slot (ties: alphabetical by
/// name), then all unslotted fragments alphabetically after every slotted one.
/// Slot is a sort key, not a uniqueness constraint: distinct names sharing a
/// slot both survive.
fn order_middles(fragments: Vec<Fragment>) -> Vec<Fragment> {
    let (mut slotted, mut unslotted): (Vec<Fragment>, Vec<Fragment>) =
        fragments.into_iter().partition(|f| f.slot.is_some());
    slotted.sort_by(|a, b| a.slot.cmp(&b.slot).then_with(|| a.name.cmp(&b.name)));
    unslotted.sort_by(|a, b| a.name.cmp(&b.name));
    slotted.into_iter().chain(unslotted).collect()
}

/// Generated `available_tools` body: `Available tools:` plus one
/// `- {name}: {description}` line per registered tool (registration order),
/// then the "no other tools" guardrail.
fn available_tools_body(tools: &Tools) -> String {
    let mut body = String::from("Available tools:");
    for tool in tools.iter() {
        body.push_str(&format!("\n- {}: {}", tool.name(), tool.description()));
    }
    body.push_str(
        "\n\nNo other tools are available. Do not assume Codex CLI/native agent tools, \
multi_tool wrappers, subagents, or hidden parallel tool APIs exist.",
    );
    body
}

/// Generated `available_tool_guidelines` body: `Guidelines:` plus a
/// tool-conditional file-inspection bullet (when the read family is present)
/// and the always-include bullets, regenerated from the live tool set.
fn tool_guidelines_body(tools: &Tools) -> String {
    let names: HashSet<&str> = tools.iter().map(|tool| tool.name()).collect();
    let mut bullets: Vec<&str> = Vec::new();
    if ["read", "grep", "find", "ls"]
        .iter()
        .all(|n| names.contains(n))
    {
        bullets.push(
            "Prefer read, grep, find, and ls for file inspection; use bash for shell commands and verification.",
        );
    } else if names.contains("bash") {
        bullets.push("Use bash for file operations like ls, rg, find");
    }
    bullets.push("Be concise in your responses");
    bullets.push("Show file paths clearly when working with files");

    let mut body = String::from("Guidelines:");
    for bullet in bullets {
        body.push_str(&format!("\n- {bullet}"));
    }
    body
}

/// Wrap the discovered project docs in pi's `<project_context>` envelope, one
/// `<project_instructions path="...">` per file. `None` when there are no docs.
fn project_context_block(docs: &[(String, String)]) -> Option<String> {
    if docs.is_empty() {
        return None;
    }
    let mut block =
        String::from("<project_context>\n\nProject-specific instructions and guidelines:\n");
    for (path, content) in docs {
        block.push_str(&format!(
            "\n<project_instructions path=\"{path}\">\n{}\n</project_instructions>\n",
            content.trim()
        ));
    }
    block.push_str("\n</project_context>");
    Some(block)
}

/// The trailing runtime-context lines (pi-style plain text, not a tagged block):
/// the current date and the working directory (backslashes normalized to `/`).
fn runtime_context_block(workspace: &Path, date: &str) -> String {
    let cwd = workspace.display().to_string().replace('\\', "/");
    format!("Current date: {date}\nCurrent working directory: {cwd}")
}

/// The shipped in-binary fragments, the single fragment source (ADR-0026).
fn default_fragments() -> Vec<Fragment> {
    DEFAULTS
        .iter()
        .map(|d| Fragment {
            name: d.name.to_string(),
            slot: d.slot,
            body: d.body.trim().to_string(),
        })
        .collect()
}

/// Discover project docs walking `cwd` -> filesystem root, deduping by path, and
/// returning them root-to-leaf (pi order). Each directory contributes its first
/// existing, non-empty, regular-file candidate.
fn discover_project_docs(cwd: &Path) -> Vec<(String, String)> {
    // Each iteration shortens `current` via `parent()`, so every visited dir --
    // and therefore every candidate path -- is unique; no dedup set is needed.
    let mut docs = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        if let Some(doc) = read_doc_in_dir(&current) {
            docs.push(doc);
        }
        match current.parent() {
            Some(parent) => current = parent.to_path_buf(),
            None => break,
        }
    }
    docs.reverse(); // leaf-first collection -> root-to-leaf emission
    docs
}

/// Read the first existing, non-empty project doc in `dir`. Only a regular file
/// is read: a symlink is rejected (`symlink_metadata` does not follow it), so a
/// planted symlink cannot exfiltrate a host file into the prompt. The read is
/// bounded by [`MAX_DOC_BYTES`].
fn read_doc_in_dir(dir: &Path) -> Option<(String, String)> {
    for candidate in DOC_CANDIDATES {
        let path = dir.join(candidate);
        let Some(content) = read_regular_bounded(&path, MAX_DOC_BYTES) else {
            continue;
        };
        if content.trim().is_empty() {
            continue;
        }
        return Some((path.display().to_string(), content));
    }
    None
}

/// Read a regular file's first `max` bytes as lossy UTF-8, refusing symlinks.
/// The exfiltration guard (a planted symlink to a host secret) applies to every
/// file folded into the prompt. `symlink_metadata` rejects a symlink/non-regular
/// entry before opening, and the final component is opened with `O_NOFOLLOW`
/// (Unix) so a check/open race cannot swap a regular file for a symlink between
/// the type check and the read. `None` on any miss.
fn read_regular_bounded(path: &Path, max: u64) -> Option<String> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => {}
        _ => return None,
    }
    let file = open_no_follow(path).ok()?;
    if !file.metadata().map(|m| m.is_file()).unwrap_or(false) {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Open a file for reading without following a final-component symlink.
#[cfg(unix)]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

// Iris targets Linux; this arm exists only so the crate still compiles on
// non-Unix hosts. It is not a hardened path (no reparse-point handling), which
// is acceptable because Iris does not ship a Windows target.
#[cfg(not(unix))]
fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Today's date as `YYYY-MM-DD` (UTC), pi-style.
fn today_ymd() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Convert a count of days since the Unix epoch (1970-01-01) to a civil
/// `(year, month, day)`.
//
// ponytail: handrolled civil-date conversion. std has no calendar/date
// formatting, and adding `chrono`/`time` for one `YYYY-MM-DD` string is
// disproportionate (library-reuse rule). This is Howard Hinnant's public-domain
// `civil_from_days` algorithm, ~6 lines, proleptic Gregorian, covered by a
// self-check test. Swap for a date crate only if real date math is ever needed.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (y + i64::from(m <= 2), m, d)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
