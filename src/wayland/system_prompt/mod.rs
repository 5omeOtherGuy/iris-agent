//! Tier-2 harness-owned system prompt assembly: a fragment/slot "baukasten".
//!
//! The provider-visible instruction string is composed from user-droppable
//! `.md` fragment files plus auto-injected dynamic context, re-implementing
//! pi-agent's system-prompt behavior. Each fragment is one XML block; its
//! frontmatter `name` is the tag and its body is the inner text. Fragments load
//! from a global dir (`~/.iris/fragments`) and a per-repo dir
//! (`<workspace>/.iris/fragments`); a user adds, edits, or reorders blocks
//! purely by dropping files, the same UX as Claude Code agents/skills.
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
//! 1. `identity` (anchored first; authored, loaded from a fragment file),
//! 2. middle fragments: slotted by ascending `slot` (same slot: global-source
//!    before repo-source, then alphabetical by `name`), then unslotted
//!    fragments alphabetically,
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
//! ## Frontmatter keys
//!
//! `name` is the xml tag and `slot` the ordering key (`0` = disabled). Every
//! other key -- including `description`, the one-line intent the shipped
//! defaults carry for humans/agents reading the file -- is metadata only: it is
//! ignored by the parser and never rendered into the prompt (forward-compat).
//!
//! ## Purity
//!
//! [`assemble`] is read-only (it never writes), and the core [`build_prompt`] is
//! a pure function of its inputs, so per-turn re-assembly (deferred) is a later
//! no-restructure change: call it again with fresh dynamic context.
//! Materialization of the shipped defaults is the one side effect, isolated in
//! [`ensure_default_fragments`] and run once at startup.
//!
//! ## Path safety
//!
//! Project-doc discovery walks cwd -> filesystem root reading `AGENTS.md` /
//! `CLAUDE.md` like pi/Codex. Ancestor docs are treated as user-owned trusted
//! config (the same trust class as `~/.iris/settings.json`, which the harness
//! already reads from `HOME`): a normal cloned repo only controls files inside
//! the workspace, not ancestor directories. Every file folded into the prompt
//! (project docs AND fragment files) is read through [`read_regular_bounded`],
//! which refuses symlinks (via `symlink_metadata`), opens the final component
//! with `O_NOFOLLOW` to close the check/open race, and caps the bytes read. So a
//! cloned repo cannot plant `AGENTS.md -> ~/.ssh/id_rsa` (or a symlinked
//! `.iris/fragments/*.md`) to exfiltrate host files into the prompt.

mod defaults;

use std::collections::{HashMap, HashSet};
use std::env;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::nexus::Tools;

use defaults::{DEFAULTS, Default};

/// Anchored fragment name pinned first.
const ANCHOR_IDENTITY: &str = "identity";
/// Anchored fragment name pinned last (authored tool guidance).
const ANCHOR_TOOL_USE: &str = "tool_use";
/// Generated tool-tail block: the live tool list.
const GEN_AVAILABLE_TOOLS: &str = "available_tools";
/// Generated tool-tail block: the tool guidelines.
const GEN_TOOL_GUIDELINES: &str = "available_tool_guidelines";

/// Project-doc filenames discovered per directory, in priority order (first
/// existing wins for that directory). Mirrors pi's candidate list.
const DOC_CANDIDATES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Upper bound on bytes folded into the prompt per discovered project doc, so a
/// runaway or hostile file cannot balloon every request / OOM the process.
const MAX_DOC_BYTES: u64 = 32 * 1024;

/// Upper bound on bytes read per fragment file, so a committed huge fragment
/// cannot bloat every provider request.
const MAX_FRAGMENT_BYTES: u64 = 64 * 1024;

/// Source a fragment was loaded from. Ordering precedence: global before repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Global,
    Repo,
}

fn source_rank(source: Source) -> u8 {
    match source {
        Source::Global => 0,
        Source::Repo => 1,
    }
}

/// One parsed fragment: `name` (the xml tag), an optional `slot` sort key
/// (`Some(0)` disables the fragment), the source it came from, and the body
/// (surrounding whitespace trimmed).
#[derive(Debug, Clone)]
struct Fragment {
    name: String,
    slot: Option<u32>,
    source: Source,
    body: String,
}

/// Assemble the full provider-visible system prompt for `workspace`.
///
/// Read-only: loads the global (`~/.iris/fragments`) and repo
/// (`<workspace>/.iris/fragments`) fragment dirs, discovers project docs, and
/// builds the prompt. A missing/empty dir is normal. When no fragment file
/// exists in either dir, the in-memory shipped defaults are used so the prompt
/// is never empty. Both fresh and resumed sessions call this with the same
/// workspace, so they assemble identical instructions.
pub(crate) fn assemble(workspace: &Path, tools: &Tools) -> String {
    let mut fragments = Vec::new();
    if let Some(global) = global_fragments_dir() {
        fragments.extend(load_dir(&global, Source::Global));
    }
    fragments.extend(load_repo_fragments(workspace));
    if fragments.is_empty() {
        fragments = default_fragments();
    }

    let docs = discover_project_docs(workspace);
    build_prompt(fragments, tools, workspace, &docs, &today_ymd())
}

/// Load the per-repo fragments through the workspace path sandbox. The dir is
/// canonicalized and required to stay inside the workspace, so a clone-committed
/// `.iris` or `.iris/fragments` symlink that escapes the workspace is rejected
/// instead of followed -- otherwise `read_dir` would enumerate the symlink
/// target and fold host `.md` files into the prompt. A missing dir (the normal
/// case) and an escaping symlink both yield no fragments.
fn load_repo_fragments(workspace: &Path) -> Vec<Fragment> {
    let Ok(root) = crate::tools::path::workspace_root(workspace) else {
        return Vec::new();
    };
    let Ok(dir) = crate::tools::path::resolve_existing(&root, ".iris/fragments") else {
        return Vec::new();
    };
    load_dir(&dir, Source::Repo)
}

/// Test-only: assemble from the in-memory shipped defaults with no fragment
/// files, project docs, or `HOME`/disk access -- a hermetic instruction string
/// for provider request-shaping tests.
#[cfg(test)]
pub(crate) fn assemble_defaults(workspace: &Path, tools: &Tools) -> String {
    build_prompt(default_fragments(), tools, workspace, &[], &today_ymd())
}

/// Materialize the shipped defaults into `~/.iris/fragments` if absent. Run once
/// at startup. Best-effort: a missing `HOME` or a write failure is logged, never
/// fatal. Never overwrites an existing file, so user edits survive.
pub(crate) fn ensure_default_fragments() {
    let Some(dir) = global_fragments_dir() else {
        tracing::debug!("no HOME set; skipping default fragment materialization");
        return;
    };
    if let Err(error) = materialize_defaults(&dir) {
        tracing::warn!(
            error = %format!("{error:#}"),
            dir = %dir.display(),
            "failed to materialize default fragments"
        );
    }
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
    // are never loaded from files, so a user-authored copy is dropped.
    let identity = take_anchor(&mut fragments, ANCHOR_IDENTITY);
    let tool_use = take_anchor(&mut fragments, ANCHOR_TOOL_USE);
    fragments.retain(|f| f.name != GEN_AVAILABLE_TOOLS && f.name != GEN_TOOL_GUIDELINES);
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

/// Remove every fragment named `name` and return the winning body. Repo source
/// overrides global for an anchored singleton; `None` when no such fragment
/// exists. Removing all matches keeps a stray duplicate out of the middles.
fn take_anchor(fragments: &mut Vec<Fragment>, name: &str) -> Option<String> {
    let mut winner: Option<usize> = None;
    for (i, fragment) in fragments.iter().enumerate() {
        if fragment.name != name {
            continue;
        }
        match winner {
            None => winner = Some(i),
            Some(j) if source_rank(fragment.source) >= source_rank(fragments[j].source) => {
                winner = Some(i)
            }
            Some(_) => {}
        }
    }
    let body = winner.map(|i| fragments[i].body.clone());
    fragments.retain(|fragment| fragment.name != name);
    body
}

/// Collapse same-name fragments to one winner (repo beats global; among equal
/// source the later one wins, matching [`take_anchor`]). Keeps the winner's own
/// slot and body. Distinct names are untouched even when they share a slot.
fn dedup_by_name(fragments: Vec<Fragment>) -> Vec<Fragment> {
    let mut winner: HashMap<&str, usize> = HashMap::new();
    for (i, fragment) in fragments.iter().enumerate() {
        match winner.get(fragment.name.as_str()) {
            Some(&j) if source_rank(fragment.source) < source_rank(fragments[j].source) => {}
            _ => {
                winner.insert(fragment.name.as_str(), i);
            }
        }
    }
    let keep: HashSet<usize> = winner.into_values().collect();
    fragments
        .into_iter()
        .enumerate()
        .filter_map(|(i, f)| keep.contains(&i).then_some(f))
        .collect()
}

/// Order the middle fragments: slotted by ascending slot (ties: global before
/// repo, then alphabetical by name), then all unslotted fragments alphabetically
/// after every slotted one.
///
/// Dedup is by `name` only (never by slot): same-name fragments collapse to one
/// so a repo `.iris/fragments/foo.md` overrides the materialized global default
/// of the same name, matching how [`take_anchor`] dedups anchors. The winner
/// keeps its own slot and body, so its position follows its own slot. Repo beats
/// global; among same-name same-source ties the later fragment wins (same
/// "later wins" rule as [`take_anchor`]). Distinct names sharing a slot both
/// survive -- slot is a sort key, not a uniqueness constraint.
fn order_middles(fragments: Vec<Fragment>) -> Vec<Fragment> {
    let (mut slotted, mut unslotted): (Vec<Fragment>, Vec<Fragment>) = dedup_by_name(fragments)
        .into_iter()
        .partition(|f| f.slot.is_some());
    slotted.sort_by(|a, b| {
        a.slot
            .cmp(&b.slot)
            .then_with(|| source_rank(a.source).cmp(&source_rank(b.source)))
            .then_with(|| a.name.cmp(&b.name))
    });
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

/// The in-memory shipped defaults, used when no fragment file exists on disk.
fn default_fragments() -> Vec<Fragment> {
    DEFAULTS
        .iter()
        .map(|d| Fragment {
            name: d.name.to_string(),
            slot: d.slot,
            source: Source::Global,
            body: d.body.trim().to_string(),
        })
        .collect()
}

/// Load every `*.md` fragment file in `dir`. A missing dir, a non-`.md` entry,
/// a non-regular entry (symlink/dir), or a read error contributes nothing -- a
/// missing/empty fragments dir is normal, never an error.
fn load_dir(dir: &Path, source: Source) -> Vec<Fragment> {
    // Reject a symlinked fragments dir: read_dir would follow it, and the
    // per-file symlink guard cannot catch a dir whose *target* holds real
    // regular files. (For the repo dir this is belt-and-suspenders;
    // load_repo_fragments already resolved it inside the workspace sandbox.)
    if std::fs::symlink_metadata(dir)
        .map(|m| m.is_symlink())
        .unwrap_or(false)
    {
        return Vec::new();
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // A missing dir is the normal "no fragments here" case.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        // A real failure (permissions, corruption) must not silently produce a
        // bare prompt without a trace.
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                dir = %dir.display(),
                "could not read fragments dir"
            );
            return Vec::new();
        }
    };
    // Sort by path so duplicate (slot, source, name) fragments -- or duplicate
    // anchors -- resolve deterministically regardless of the OS read_dir order.
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    paths.sort();

    let mut out = Vec::new();
    for path in paths {
        let Some(raw) = read_regular_bounded(&path, MAX_FRAGMENT_BYTES) else {
            continue;
        };
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("fragment");
        out.push(parse_fragment(stem, source, &raw));
    }
    out
}

/// Parse one fragment file: optional leading `---` frontmatter followed by the
/// body. Only `name` (the xml tag) and `slot` (ordering key) are read; every
/// other key -- `description` and any future key -- is metadata, ignored here so
/// it never reaches the rendered prompt. `name` defaults to the file stem when
/// frontmatter omits it. The body is everything after the closing `---`,
/// surrounding whitespace trimmed.
fn parse_fragment(stem: &str, source: Source, raw: &str) -> Fragment {
    // Tolerate a leading UTF-8 BOM (some editors prepend one); otherwise the
    // frontmatter fence would not match and the whole file, frontmatter
    // included, would be ingested as the body.
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut name = stem.to_string();
    let mut slot = None;

    let body = match strip_frontmatter_open(raw).and_then(split_at_closing_fence) {
        Some((front, rest)) => {
            for line in front.lines() {
                let Some((key, value)) = line.split_once(':') else {
                    continue;
                };
                let value = value.trim().trim_matches(['"', '\'']).trim();
                match key.trim() {
                    "name" if !value.is_empty() => name = value.to_string(),
                    "slot" => slot = value.parse::<u32>().ok(),
                    // Everything else (description, future model/mode/... keys)
                    // is metadata: ignored so it never reaches the prompt body.
                    _ => {}
                }
            }
            rest
        }
        // No frontmatter (or no closing fence): the whole file is the body.
        None => raw,
    };

    Fragment {
        name,
        slot,
        source,
        body: body.trim().to_string(),
    }
}

/// If `raw` opens with a `---` frontmatter fence, return the text right after
/// it; otherwise `None`.
fn strip_frontmatter_open(raw: &str) -> Option<&str> {
    let rest = raw.strip_prefix("---")?;
    rest.strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
}

/// Split frontmatter content at its closing `---` line, returning
/// (frontmatter, body). `None` when there is no closing fence.
fn split_at_closing_fence(after: &str) -> Option<(&str, &str)> {
    let mut offset = 0;
    for line in after.split_inclusive('\n') {
        if line.trim_end_matches(['\n', '\r']).trim() == "---" {
            return Some((&after[..offset], &after[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
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
/// Shared by fragment files and project docs so the same exfiltration guard (a
/// planted `*.md` symlink to a host secret) applies to every file folded into
/// the prompt. `symlink_metadata` rejects a symlink/non-regular entry before
/// opening, and the final component is opened with `O_NOFOLLOW` (Unix) so a
/// check/open race cannot swap a regular file for a symlink between the type
/// check and the read. `None` on any miss.
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

/// Global fragments dir: `~/.iris/fragments`. `None` when `HOME` is unset/empty,
/// so the global layer is simply skipped (mirrors `config::global_path`).
fn global_fragments_dir() -> Option<PathBuf> {
    let home = env::var("HOME").ok().filter(|home| !home.is_empty())?;
    Some(Path::new(&home).join(".iris/fragments"))
}

/// Write each shipped default into `dir` if the file does not already exist.
fn materialize_defaults(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for default in DEFAULTS {
        let path = dir.join(format!("{}.md", default.name));
        if path.exists() {
            continue;
        }
        std::fs::write(&path, default_file_contents(default))?;
    }
    Ok(())
}

/// Render a shipped default to its on-disk `.md` form: `---` frontmatter
/// (`name`, `description`, optional `slot`) then the body. The `description`
/// makes each materialized file self-documenting without leaking into the
/// prompt.
fn default_file_contents(default: &Default) -> String {
    let mut out = format!(
        "---\nname: {}\ndescription: {}\n",
        default.name, default.description
    );
    if let Some(slot) = default.slot {
        out.push_str(&format!("slot: {slot}\n"));
    }
    out.push_str("---\n");
    out.push_str(default.body);
    out.push('\n');
    out
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
