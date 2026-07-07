//! Behavior tests for the fragment/slot system-prompt assembler. Each test
//! pins one rule from the spec: a regression in ordering, anchoring, the
//! empty-body skip, generated blocks, project-doc discovery, dynamic context,
//! or the ADR-0026 internal-only guarantee fails exactly here.

use super::*;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tools::{built_in_tools, built_in_tools_for};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    home: Option<OsString>,
}

impl EnvGuard {
    fn with_home(home: &Path) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        let guard = Self {
            _lock: lock,
            home: env::var_os("HOME"),
        };
        // SAFETY: system_prompt env-sensitive tests run under ENV_LOCK and
        // restore HOME before releasing it.
        unsafe { env::set_var("HOME", home) };
        guard
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: serialized under ENV_LOCK by EnvGuard and restored on drop.
        unsafe {
            match &self.home {
                Some(value) => env::set_var("HOME", value),
                None => env::remove_var("HOME"),
            }
        }
    }
}

struct TempDir {
    path: PathBuf,
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn temp_dir() -> TempDir {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = env::temp_dir().join(format!("iris-fragments-test-{nanos}-{seq}"));
    fs::create_dir(&path).unwrap();
    TempDir { path }
}

fn frag(name: &str, slot: Option<u32>, body: &str) -> Fragment {
    Fragment {
        name: name.to_string(),
        slot,
        body: body.to_string(),
    }
}

/// Index where a block's open tag appears, asserting it exists.
fn at(prompt: &str, tag: &str) -> usize {
    prompt
        .find(&format!("<{tag}>"))
        .unwrap_or_else(|| panic!("missing <{tag}> block in:\n{prompt}"))
}

fn build_with(frags: Vec<Fragment>, docs: &[(String, String)]) -> String {
    build_prompt(
        frags,
        &built_in_tools(),
        Path::new("/tmp/iris-ws"),
        docs,
        "2026-06-18",
    )
}

// ---- ordering rules ---------------------------------------------------------

#[test]
fn slotted_fragments_emit_in_ascending_slot_order() {
    let frags = vec![
        frag("c", Some(3), "C"),
        frag("a", Some(1), "A"),
        frag("b", Some(2), "B"),
    ];
    let names: Vec<String> = order_middles(frags).into_iter().map(|f| f.name).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn same_slot_orders_alphabetically() {
    let frags = vec![
        frag("zeta", Some(1), ""),
        frag("beta", Some(1), ""),
        frag("alpha", Some(1), ""),
    ];
    let names: Vec<String> = order_middles(frags).into_iter().map(|f| f.name).collect();
    assert_eq!(names, vec!["alpha", "beta", "zeta"]);
}

#[test]
fn unslotted_fragments_follow_all_slotted_alphabetically() {
    let frags = vec![
        frag("zzz", Some(9), ""),
        frag("banana", None, ""),
        frag("apple", None, ""),
    ];
    let names: Vec<String> = order_middles(frags).into_iter().map(|f| f.name).collect();
    assert_eq!(names, vec!["zzz", "apple", "banana"]);
}

#[test]
fn slot_is_a_sort_key_not_a_uniqueness_constraint() {
    // Two fragments sharing a slot both survive ordering (no dedup).
    let frags = vec![frag("one", Some(5), "one"), frag("two", Some(5), "two")];
    assert_eq!(order_middles(frags).len(), 2);
}

// ---- anchoring and empty-body skip ------------------------------------------

#[test]
fn identity_is_first_and_tool_tail_order_is_fixed() {
    let frags = vec![
        frag("identity", None, "I am iris."),
        frag("middle", Some(1), "a middle block"),
        frag("tool_use", None, "tool guidance"),
    ];
    let prompt = build_with(frags, &[]);
    assert!(at(&prompt, "identity") < at(&prompt, "middle"));
    assert!(at(&prompt, "middle") < at(&prompt, "available_tools"));
    assert!(at(&prompt, "available_tools") < at(&prompt, "available_tool_guidelines"));
    assert!(at(&prompt, "available_tool_guidelines") < at(&prompt, "tool_use"));
}

#[test]
fn anchors_cannot_be_repositioned_by_a_slot() {
    // identity carries a high slot and tool_use a low one; anchoring ignores
    // both, keeping identity first and tool_use in the tail. (slot 1 not 0:
    // slot 0 would disable the fragment entirely -- see slot_zero tests.)
    let frags = vec![
        frag("tool_use", Some(1), "tail prose"),
        frag("identity", Some(99), "head"),
        frag("middle", Some(50), "mid"),
    ];
    let prompt = build_with(frags, &[]);
    assert!(at(&prompt, "identity") < at(&prompt, "middle"));
    assert!(at(&prompt, "middle") < at(&prompt, "tool_use"));
}

#[test]
fn slot_zero_disables_a_middle_fragment() {
    let frags = vec![
        frag("identity", None, "id"),
        frag("off", Some(0), "SHOULD NOT APPEAR"),
        frag("on", Some(1), "present"),
    ];
    let prompt = build_with(frags, &[]);
    assert!(
        !prompt.contains("<off>"),
        "slot 0 must disable the fragment"
    );
    assert!(!prompt.contains("SHOULD NOT APPEAR"));
    assert!(prompt.contains("<on>"));
}

#[test]
fn slot_zero_disables_even_an_anchor() {
    // "Not active at all" applies uniformly: a disabled anchor emits nothing.
    let frags = vec![
        frag("identity", Some(0), "DISABLED IDENTITY"),
        frag("tool_use", Some(0), "DISABLED TAIL"),
        frag("kept", Some(1), "kept body"),
    ];
    let prompt = build_with(frags, &[]);
    assert!(!prompt.contains("<identity>"));
    assert!(!prompt.contains("DISABLED IDENTITY"));
    assert!(!prompt.contains("<tool_use>"));
    assert!(!prompt.contains("DISABLED TAIL"));
    assert!(prompt.contains("<kept>"));
    // Generated tool list is independent of the authored tool_use anchor.
    assert!(prompt.contains("<available_tools>"));
}

#[test]
fn empty_body_fragment_emits_no_block() {
    let frags = vec![
        frag("identity", None, "id"),
        frag("blank", Some(1), "   \n\t  "),
        frag("kept", Some(2), "present"),
    ];
    let prompt = build_with(frags, &[]);
    assert!(!prompt.contains("<blank>"), "empty body must emit nothing");
    assert!(prompt.contains("<kept>"));
}

#[test]
fn empty_identity_or_tool_use_anchor_emits_nothing() {
    let frags = vec![frag("identity", None, "  "), frag("tool_use", None, "")];
    let prompt = build_with(frags, &[]);
    assert!(!prompt.contains("<identity>"));
    assert!(!prompt.contains("<tool_use>"));
    // Generated tool blocks are always present.
    assert!(prompt.contains("<available_tools>"));
}

#[test]
fn stray_fragment_named_like_a_generated_block_is_dropped() {
    let frags = vec![frag("available_tools", Some(1), "FAKE TOOL LIST")];
    let prompt = build_with(frags, &[]);
    assert!(!prompt.contains("FAKE TOOL LIST"));
    assert!(prompt.contains("Available tools:"));
}

#[test]
fn take_anchor_consumes_every_match_and_last_one_wins() {
    let mut frags = vec![
        frag("identity", None, "first identity"),
        frag("identity", None, "last identity"),
    ];
    let body = take_anchor(&mut frags, "identity");
    assert_eq!(body.as_deref(), Some("last identity"));
    assert!(frags.is_empty(), "all identity fragments removed");
}

// ---- generated tool blocks reflect the registry ------------------------------

#[test]
fn available_tools_lists_every_registered_tool_with_the_guardrail() {
    let tools = built_in_tools();
    let body = available_tools_body(&tools);
    assert!(body.starts_with("Available tools:"));
    for name in [
        "read",
        "bash",
        "edit",
        "write",
        "grep",
        "find",
        "ls",
        "read_output",
        "recall",
    ] {
        assert!(body.contains(&format!("- {name}:")), "missing tool {name}");
    }
    assert!(body.contains("No other tools are available"));
    assert!(body.contains("do not assume") || body.contains("Do not assume"));
}

#[test]
fn available_tools_preserves_registration_order() {
    let tools = built_in_tools();
    let body = available_tools_body(&tools);
    let read_at = body.find("- read:").unwrap();
    let bash_at = body.find("- bash:").unwrap();
    let ls_at = body.find("- ls:").unwrap();
    assert!(read_at < bash_at && bash_at < ls_at);
}

#[test]
fn bash_tool_mode_prompt_advertises_shell_operations_not_native_file_tools() {
    let tools = built_in_tools_for(true);
    let body = available_tools_body(&tools);
    for name in ["bash", "edit", "read_output", "recall"] {
        assert!(body.contains(&format!("- {name}:")), "missing tool {name}");
    }
    for gone in ["read", "write", "grep", "find", "ls"] {
        assert!(
            !body.contains(&format!("- {gone}:")),
            "{gone} should be hidden"
        );
    }
    let guidelines = tool_guidelines_body(&tools);
    assert!(guidelines.contains("Use bash for file operations like ls, rg, find"));
    assert!(!guidelines.contains("Prefer read, grep, find, and ls"));
}

#[test]
fn recall_fragment_renders_only_when_the_recall_tool_is_registered() {
    let workspace = Path::new("/tmp/recall-fragment-test");
    // Registered (the full built-in set includes recall): the fragment renders,
    // documenting the marker and the `recall(handle=...)` affordance.
    let with_recall = assemble_defaults(workspace, &built_in_tools());
    assert!(
        with_recall.contains("<compaction_recall>"),
        "recall fragment must render when the tool is registered"
    );
    assert!(with_recall.contains("recall(handle="));
    // Not registered: the fragment must be absent, so no build advertises an
    // affordance the model cannot invoke (ADR-0046 / ADR-0014).
    let without_recall = assemble_defaults(workspace, &Tools::new(Vec::new()));
    assert!(
        !without_recall.contains("<compaction_recall>"),
        "recall fragment must not render when the tool is absent"
    );
}

#[test]
fn tool_guidelines_have_conditional_and_always_bullets() {
    let body = tool_guidelines_body(&built_in_tools());
    assert!(body.starts_with("Guidelines:"));
    assert!(body.contains("Prefer read, grep, find, and ls for file inspection"));
    assert!(body.contains("Be concise in your responses"));
    assert!(body.contains("Show file paths clearly when working with files"));
}

// ---- dynamic context (project docs + date/cwd) --------------------------------

#[test]
fn project_docs_render_in_a_project_context_block_root_to_leaf() {
    let docs = vec![
        ("/root/AGENTS.md".to_string(), "root rules".to_string()),
        ("/root/app/AGENTS.md".to_string(), "leaf rules".to_string()),
    ];
    let block = project_context_block(&docs).expect("docs present");
    assert!(block.starts_with("<project_context>"));
    assert!(block.contains("<project_instructions path=\"/root/AGENTS.md\">"));
    assert!(block.contains("<project_instructions path=\"/root/app/AGENTS.md\">"));
    let root_at = block.find("root rules").unwrap();
    let leaf_at = block.find("leaf rules").unwrap();
    assert!(root_at < leaf_at, "root-to-leaf order");
}

#[test]
fn no_docs_yields_no_project_context_block() {
    assert!(project_context_block(&[]).is_none());
}

#[test]
fn date_and_cwd_lines_precede_the_tool_tail_with_backslash_normalization() {
    let prompt = build_prompt(
        vec![frag("identity", None, "id")],
        &built_in_tools(),
        Path::new("C:\\Users\\dev\\proj"),
        &[],
        "2026-06-18",
    );
    assert!(prompt.contains("Current date: 2026-06-18"));
    assert!(prompt.contains("Current working directory: C:/Users/dev/proj"));
    let cwd_at = prompt.find("Current working directory:").unwrap();
    assert!(
        cwd_at < at(&prompt, "available_tools"),
        "cwd before tool tail"
    );
}

#[test]
fn project_context_appears_between_middles_and_runtime_context() {
    let docs = vec![("/ws/AGENTS.md".to_string(), "be terse".to_string())];
    let prompt = build_prompt(
        vec![frag("identity", None, "id"), frag("mid", Some(1), "m")],
        &built_in_tools(),
        Path::new("/ws"),
        &docs,
        "2026-06-18",
    );
    let mid_at = at(&prompt, "mid");
    let pc_at = prompt.find("<project_context>").unwrap();
    let date_at = prompt.find("Current date:").unwrap();
    assert!(mid_at < pc_at && pc_at < date_at);
}

// ---- discovery (filesystem) -----------------------------------------------

#[test]
fn discover_walks_cwd_to_root_and_orders_root_to_leaf() {
    let root = temp_dir();
    let leaf = root.path.join("a/b");
    fs::create_dir_all(&leaf).unwrap();
    fs::write(root.path.join("AGENTS.md"), "ROOT").unwrap();
    fs::write(leaf.join("AGENTS.md"), "LEAF").unwrap();

    let docs = discover_project_docs(&leaf);
    let bodies: Vec<&str> = docs.iter().map(|(_, c)| c.as_str()).collect();
    let root_pos = bodies.iter().position(|c| *c == "ROOT").unwrap();
    let leaf_pos = bodies.iter().position(|c| *c == "LEAF").unwrap();
    assert!(root_pos < leaf_pos, "root-to-leaf");
}

#[test]
fn discover_prefers_agents_md_over_claude_md_per_dir() {
    let dir = temp_dir();
    fs::write(dir.path.join("AGENTS.md"), "AGENTS").unwrap();
    fs::write(dir.path.join("CLAUDE.md"), "CLAUDE").unwrap();
    let docs = discover_project_docs(&dir.path);
    let here: Vec<&str> = docs
        .iter()
        .filter(|(p, _)| p.contains(&dir.path.display().to_string()))
        .map(|(_, c)| c.as_str())
        .collect();
    assert!(here.contains(&"AGENTS"));
    assert!(!here.contains(&"CLAUDE"));
}

#[test]
fn discover_skips_empty_project_doc() {
    let dir = temp_dir();
    fs::write(dir.path.join("AGENTS.md"), "   \n\t\n").unwrap();
    let docs = discover_project_docs(&dir.path);
    assert!(
        docs.iter()
            .all(|(p, _)| !p.contains("AGENTS.md")
                || !p.starts_with(&dir.path.display().to_string()))
    );
}

#[cfg(unix)]
#[test]
fn discover_rejects_a_symlinked_project_doc() {
    use std::os::unix::fs::symlink;
    let outside = temp_dir();
    let secret = outside.path.join("secret.txt");
    fs::write(&secret, "TOP SECRET HOST FILE").unwrap();

    let ws = temp_dir();
    symlink(&secret, ws.path.join("AGENTS.md")).unwrap();

    let docs = discover_project_docs(&ws.path);
    assert!(
        docs.iter()
            .all(|(_, c)| !c.contains("TOP SECRET HOST FILE")),
        "a symlinked AGENTS.md must not be read"
    );
}

// ---- ADR-0026: fragments are internal-only ---------------------------------

#[test]
fn assemble_ignores_repo_fragments_entirely() {
    // ADR-0026: a repo `.iris/fragments/*.md` is never folded into the prompt.
    // There is no trust decision to make -- even a workspace that was "trusted"
    // under the old gate assembles from the internal defaults only.
    let ws = temp_dir();
    fs::create_dir_all(ws.path.join(".iris/fragments")).unwrap();
    fs::write(
        ws.path.join(".iris/fragments/injected.md"),
        "---\nname: injected\nslot: 5\n---\nHOSTILE FRAGMENT",
    )
    .unwrap();
    fs::write(ws.path.join("AGENTS.md"), "PROJECT DOC CONTENT").unwrap();

    let prompt = assemble(&ws.path, &built_in_tools());
    assert!(
        !prompt.contains("HOSTILE FRAGMENT"),
        "repo fragments must never load"
    );
    assert!(!prompt.contains("<injected>"));
    assert!(
        prompt.contains("PROJECT DOC CONTENT"),
        "project docs still fold in"
    );
    // The full internal prompt is present.
    assert!(prompt.contains("<identity>"));
    assert!(prompt.contains("<available_tools>"));
}

#[test]
fn assemble_builds_from_internal_defaults() {
    let ws = temp_dir();
    let prompt = assemble(&ws.path, &built_in_tools());
    assert!(prompt.contains("<identity>"));
    assert!(prompt.contains("You are iris, a coding assistant"));
    assert!(prompt.contains("<available_tools>"));
    assert!(prompt.contains("<tool_use>"));
}

#[test]
fn assemble_ignores_global_home_fragments_and_writes_nothing_to_home() {
    let home = temp_dir();
    let _env = EnvGuard::with_home(&home.path);
    let global = home.path.join(".iris/fragments");
    fs::create_dir_all(&global).unwrap();
    fs::write(
        global.join("identity.md"),
        "---\nname: identity\n---\nGLOBAL HOSTILE IDENTITY",
    )
    .unwrap();
    let before = fs::read_to_string(global.join("identity.md")).unwrap();

    let ws = temp_dir();
    let prompt = assemble(&ws.path, &built_in_tools());

    assert!(
        !prompt.contains("GLOBAL HOSTILE IDENTITY"),
        "global ~/.iris/fragments must not be read"
    );
    assert_eq!(
        fs::read_to_string(global.join("identity.md")).unwrap(),
        before,
        "assemble must not materialize or rewrite HOME fragments"
    );
    assert!(
        !home.path.join(".iris/fragments/tool_use.md").exists(),
        "assemble must not materialize shipped fragments into HOME"
    );
}

#[test]
fn assemble_writes_nothing_to_the_workspace() {
    // assemble is read-only: no materialization side effect anywhere. (Startup
    // materialization into ~/.iris/fragments was removed with ADR-0026; the
    // function no longer exists, so HOME is never written either.)
    let ws = temp_dir();
    let before: Vec<PathBuf> = fs::read_dir(&ws.path)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    let _ = assemble(&ws.path, &built_in_tools());
    let after: Vec<PathBuf> = fs::read_dir(&ws.path)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(before, after, "assemble must not create files");
}

#[test]
fn shipped_identity_is_one_sentence_and_mission_follows_it() {
    // identity is the short standalone block; the goal/refine/own-your-output
    // prose lives in a separate mission fragment right after it.
    let prompt = build_prompt(
        default_fragments(),
        &built_in_tools(),
        Path::new("/tmp/iris"),
        &[],
        "2026-06-18",
    );
    let id = prompt
        .split_once("<identity>\n")
        .and_then(|(_, r)| r.split_once("\n</identity>"))
        .map(|(b, _)| b)
        .expect("identity block present");
    assert_eq!(
        id,
        "You are iris, a coding assistant collaborating with the user in this workspace on coding tasks."
    );
    // mission carries the goal and sits between identity and response_style.
    assert!(prompt.contains("<mission>"));
    assert!(prompt.contains("Your main goal: execute the user's instructions"));
    assert!(at(&prompt, "identity") < at(&prompt, "mission"));
    assert!(at(&prompt, "mission") < at(&prompt, "response_style"));
}

#[test]
fn assemble_with_only_defaults_is_equivalent_to_today_prompt_content() {
    // Definition of done: identity + tool list from the registry + the
    // no-other-tools guardrail + guidelines + cwd, all present.
    let prompt = build_prompt(
        default_fragments(),
        &built_in_tools(),
        Path::new("/tmp/iris"),
        &[],
        "2026-06-18",
    );
    assert!(prompt.contains("<identity>"));
    assert!(prompt.contains("- read:") && prompt.contains("- ls:"));
    assert!(prompt.contains("No other tools are available"));
    assert!(prompt.contains("Be concise in your responses"));
    assert!(prompt.contains("Current working directory: /tmp/iris"));
    // Shipped middle fragments appear in slot order.
    assert!(at(&prompt, "response_style") < at(&prompt, "file_links"));
}

#[test]
fn rendering_wraps_each_block_in_its_tag_separated_by_blank_lines() {
    let frags = vec![
        frag("identity", None, "id body"),
        frag("alpha", Some(1), "alpha body"),
    ];
    let prompt = build_with(frags, &[]);
    assert!(prompt.contains("<identity>\nid body\n</identity>"));
    assert!(prompt.contains("<alpha>\nalpha body\n</alpha>"));
    assert!(
        prompt.contains("</identity>\n\n<alpha>"),
        "blank line separator"
    );
}

// ---- date helper ----------------------------------------------------------

#[test]
fn civil_from_days_matches_known_dates() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    // 2000-03-01 is 11017 days after the epoch (verified against the inverse).
    assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    // 2021-01-01 is 18628 days after the epoch.
    assert_eq!(civil_from_days(18_628), (2021, 1, 1));
}
