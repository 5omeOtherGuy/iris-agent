//! Behavior tests for the fragment/slot system-prompt assembler. Each test
//! pins one rule from the spec: a regression in parsing, ordering, anchoring,
//! the empty-body skip, generated blocks, project-doc discovery, or dynamic
//! context fails exactly here.

use super::*;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::tools::built_in_tools;

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

fn frag(name: &str, slot: Option<u32>, source: Source, body: &str) -> Fragment {
    Fragment {
        name: name.to_string(),
        slot,
        source,
        body: body.to_string(),
    }
}

/// Index where a block's open tag appears, asserting it exists.
fn at(prompt: &str, tag: &str) -> usize {
    prompt
        .find(&format!("<{tag}>"))
        .unwrap_or_else(|| panic!("missing <{tag}> block in:\n{prompt}"))
}

// ---- Unit 1: frontmatter parse + body extraction --------------------------

#[test]
fn parses_name_and_slot_from_frontmatter() {
    let f = parse_fragment(
        "file",
        Source::Repo,
        "---\nname: greeting\nslot: 3\n---\nHello body",
    );
    assert_eq!(f.name, "greeting");
    assert_eq!(f.slot, Some(3));
    assert_eq!(f.body, "Hello body");
}

#[test]
fn unknown_frontmatter_keys_are_ignored() {
    // Forward-compat: future model/mode/thinking_level keys must not break parse.
    let raw = "---\nname: x\nslot: 2\nmodel: gpt-5\nthinking_level: high\n---\nbody";
    let f = parse_fragment("file", Source::Global, raw);
    assert_eq!(f.name, "x");
    assert_eq!(f.slot, Some(2));
    assert_eq!(f.body, "body");
}

#[test]
fn missing_name_defaults_to_file_stem() {
    let f = parse_fragment("my_fragment", Source::Repo, "---\nslot: 1\n---\nb");
    assert_eq!(f.name, "my_fragment");
    assert_eq!(f.slot, Some(1));
}

#[test]
fn no_frontmatter_uses_whole_file_as_body() {
    let f = parse_fragment("stem", Source::Repo, "just body text\nsecond line");
    assert_eq!(f.name, "stem");
    assert_eq!(f.slot, None);
    assert_eq!(f.body, "just body text\nsecond line");
}

#[test]
fn bom_prefixed_fragment_still_parses_frontmatter() {
    // A UTF-8 BOM from an editor must not push the frontmatter fence out of
    // alignment and dump the whole file into the body.
    let f = parse_fragment(
        "stem",
        Source::Repo,
        "\u{feff}---\nname: x\nslot: 2\n---\nbody",
    );
    assert_eq!(f.name, "x");
    assert_eq!(f.slot, Some(2));
    assert_eq!(f.body, "body");
}

#[test]
fn unparsable_slot_is_treated_as_unslotted() {
    let f = parse_fragment("stem", Source::Repo, "---\nname: x\nslot: high\n---\nb");
    assert_eq!(f.slot, None);
}

#[test]
fn slot_zero_parses_as_some_zero() {
    // The off switch is a real slot value, not a parse failure.
    let f = parse_fragment("stem", Source::Repo, "---\nname: x\nslot: 0\n---\nb");
    assert_eq!(f.slot, Some(0));
}

#[test]
fn description_frontmatter_is_ignored_and_kept_out_of_the_body() {
    // A description line is metadata: it must not bleed into the rendered body.
    let raw = "---\nname: custom\nslot: 1\ndescription: why this exists\n---\nactual body";
    let f = parse_fragment("file", Source::Repo, raw);
    assert_eq!(f.body, "actual body");
    assert_eq!(f.name, "custom");
}

#[test]
fn body_preserves_internal_blank_lines() {
    let f = parse_fragment(
        "stem",
        Source::Repo,
        "---\nname: x\n---\npara one\n\npara two\n",
    );
    assert_eq!(f.body, "para one\n\npara two");
}

// ---- Unit 3: ordering rules -----------------------------------------------

#[test]
fn slotted_fragments_emit_in_ascending_slot_order() {
    let frags = vec![
        frag("c", Some(3), Source::Repo, "C"),
        frag("a", Some(1), Source::Repo, "A"),
        frag("b", Some(2), Source::Repo, "B"),
    ];
    let names: Vec<String> = order_middles(frags).into_iter().map(|f| f.name).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn same_slot_orders_global_before_repo_then_alphabetical() {
    let frags = vec![
        frag("zeta", Some(1), Source::Global, ""),
        frag("beta", Some(1), Source::Repo, ""),
        frag("alpha", Some(1), Source::Repo, ""),
        frag("gamma", Some(1), Source::Global, ""),
    ];
    let names: Vec<String> = order_middles(frags).into_iter().map(|f| f.name).collect();
    // Global first (gamma, zeta), then repo (alpha, beta); each group alphabetical.
    assert_eq!(names, vec!["gamma", "zeta", "alpha", "beta"]);
}

#[test]
fn unslotted_fragments_follow_all_slotted_alphabetically() {
    let frags = vec![
        frag("zzz", Some(9), Source::Repo, ""),
        frag("banana", None, Source::Repo, ""),
        frag("apple", None, Source::Global, ""),
    ];
    let names: Vec<String> = order_middles(frags).into_iter().map(|f| f.name).collect();
    assert_eq!(names, vec!["zzz", "apple", "banana"]);
}

#[test]
fn slot_is_a_sort_key_not_a_uniqueness_constraint() {
    // Two fragments sharing a slot both survive ordering (no dedup).
    let frags = vec![
        frag("one", Some(5), Source::Global, "one"),
        frag("two", Some(5), Source::Global, "two"),
    ];
    assert_eq!(order_middles(frags).len(), 2);
}

#[test]
fn repo_fragment_overrides_same_name_global() {
    // A repo .iris/fragments/pragmatism_and_scope.md overrides the materialized
    // global default of the same name: one block, repo body wins.
    let frags = vec![
        frag(
            "pragmatism_and_scope",
            Some(2),
            Source::Global,
            "global body",
        ),
        frag("pragmatism_and_scope", Some(2), Source::Repo, "repo body"),
    ];
    let kept = order_middles(frags);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].body, "repo body");

    // End to end: exactly one rendered block carries the tag, with the repo body.
    let prompt = build_with(
        vec![
            frag("identity", None, Source::Global, "I am iris."),
            frag(
                "pragmatism_and_scope",
                Some(2),
                Source::Global,
                "global body",
            ),
            frag("pragmatism_and_scope", Some(2), Source::Repo, "repo body"),
        ],
        &[],
    );
    assert_eq!(prompt.matches("<pragmatism_and_scope>").count(), 1);
    assert!(prompt.contains("repo body"));
    assert!(!prompt.contains("global body"));
}

// ---- Unit 4 + 5: anchoring and empty-body skip ----------------------------

fn build_with(frags: Vec<Fragment>, docs: &[(String, String)]) -> String {
    build_prompt(
        frags,
        &built_in_tools(),
        Path::new("/tmp/iris-ws"),
        docs,
        "2026-06-18",
    )
}

#[test]
fn identity_is_first_and_tool_tail_order_is_fixed() {
    let frags = vec![
        frag("identity", None, Source::Global, "I am iris."),
        frag("middle", Some(1), Source::Global, "a middle block"),
        frag("tool_use", None, Source::Global, "tool guidance"),
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
        frag("tool_use", Some(1), Source::Global, "tail prose"),
        frag("identity", Some(99), Source::Global, "head"),
        frag("middle", Some(50), Source::Global, "mid"),
    ];
    let prompt = build_with(frags, &[]);
    assert!(at(&prompt, "identity") < at(&prompt, "middle"));
    assert!(at(&prompt, "middle") < at(&prompt, "tool_use"));
}

#[test]
fn slot_zero_disables_a_middle_fragment() {
    let frags = vec![
        frag("identity", None, Source::Global, "id"),
        frag("off", Some(0), Source::Global, "SHOULD NOT APPEAR"),
        frag("on", Some(1), Source::Global, "present"),
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
        frag("identity", Some(0), Source::Global, "DISABLED IDENTITY"),
        frag("tool_use", Some(0), Source::Global, "DISABLED TAIL"),
        frag("kept", Some(1), Source::Global, "kept body"),
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
fn description_is_never_rendered_into_the_prompt() {
    let f = parse_fragment(
        "file",
        Source::Repo,
        "---\nname: custom\nslot: 1\ndescription: SECRET INTENT NOTE\n---\nvisible body",
    );
    let prompt = build_with(vec![frag("identity", None, Source::Global, "id"), f], &[]);
    assert!(prompt.contains("visible body"));
    assert!(
        !prompt.contains("SECRET INTENT NOTE"),
        "description is metadata and must not leak into the prompt"
    );
}

#[test]
fn empty_body_fragment_emits_no_block() {
    let frags = vec![
        frag("identity", None, Source::Global, "id"),
        frag("blank", Some(1), Source::Global, "   \n\t  "),
        frag("kept", Some(2), Source::Global, "present"),
    ];
    let prompt = build_with(frags, &[]);
    assert!(!prompt.contains("<blank>"), "empty body must emit nothing");
    assert!(prompt.contains("<kept>"));
}

#[test]
fn empty_identity_or_tool_use_anchor_emits_nothing() {
    let frags = vec![
        frag("identity", None, Source::Global, "  "),
        frag("tool_use", None, Source::Global, ""),
    ];
    let prompt = build_with(frags, &[]);
    assert!(!prompt.contains("<identity>"));
    assert!(!prompt.contains("<tool_use>"));
    // Generated tool blocks are always present.
    assert!(prompt.contains("<available_tools>"));
}

#[test]
fn user_authored_generated_block_is_dropped_in_favor_of_the_generated_one() {
    let frags = vec![frag(
        "available_tools",
        Some(1),
        Source::Repo,
        "FAKE TOOL LIST",
    )];
    let prompt = build_with(frags, &[]);
    assert!(!prompt.contains("FAKE TOOL LIST"));
    assert!(prompt.contains("Available tools:"));
}

#[test]
fn repo_anchor_overrides_global_anchor() {
    let mut frags = vec![
        frag("identity", None, Source::Global, "global identity"),
        frag("identity", None, Source::Repo, "repo identity"),
    ];
    let body = take_anchor(&mut frags, "identity");
    assert_eq!(body.as_deref(), Some("repo identity"));
    assert!(frags.is_empty(), "all identity fragments removed");
}

// ---- Unit 6: generated tool blocks reflect the registry -------------------

#[test]
fn available_tools_lists_every_registered_tool_with_the_guardrail() {
    let tools = built_in_tools();
    let body = available_tools_body(&tools);
    assert!(body.starts_with("Available tools:"));
    for name in ["read", "bash", "edit", "write", "grep", "find", "ls"] {
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
fn tool_guidelines_have_conditional_and_always_bullets() {
    let body = tool_guidelines_body(&built_in_tools());
    assert!(body.starts_with("Guidelines:"));
    assert!(body.contains("Prefer read, grep, find, and ls for file inspection"));
    assert!(body.contains("Be concise in your responses"));
    assert!(body.contains("Show file paths clearly when working with files"));
}

// ---- Unit 7: dynamic context (project docs + date/cwd) --------------------

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
        vec![frag("identity", None, Source::Global, "id")],
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
        vec![
            frag("identity", None, Source::Global, "id"),
            frag("mid", Some(1), Source::Global, "m"),
        ],
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

// ---- loader + materialization + fallback ----------------------------------

#[test]
fn load_dir_missing_is_empty_not_an_error() {
    let dir = temp_dir();
    let frags = load_dir(&dir.path.join("does-not-exist"), Source::Repo);
    assert!(frags.is_empty());
}

#[cfg(unix)]
#[test]
fn load_dir_rejects_a_symlinked_fragments_dir() {
    use std::os::unix::fs::symlink;
    // A directory of real .md files that a dir-symlink must not expose.
    let secrets = temp_dir();
    fs::write(
        secrets.path.join("leak.md"),
        "---\nname: leak\n---\nHOST SECRET",
    )
    .unwrap();

    let parent = temp_dir();
    let link = parent.path.join("fragments");
    symlink(&secrets.path, &link).unwrap();
    let frags = load_dir(&link, Source::Repo);
    assert!(
        frags.is_empty(),
        "a symlinked fragments dir must not be enumerated"
    );
}

#[cfg(unix)]
#[test]
fn assemble_rejects_a_repo_fragments_dir_escaping_the_workspace() {
    use std::os::unix::fs::symlink;
    let secrets = temp_dir();
    fs::write(
        secrets.path.join("leak.md"),
        "---\nname: leak\n---\nHOST SECRET",
    )
    .unwrap();

    let ws = temp_dir();
    fs::create_dir_all(ws.path.join(".iris")).unwrap();
    // .iris/fragments -> a directory outside the workspace.
    symlink(&secrets.path, ws.path.join(".iris/fragments")).unwrap();

    let prompt = assemble(&ws.path, &built_in_tools());
    assert!(
        !prompt.contains("HOST SECRET"),
        "an escaping repo fragments dir must not be read into the prompt"
    );
    // Still a complete prompt (shipped defaults / global), never a leak.
    assert!(prompt.contains("<identity>"));
}

#[cfg(unix)]
#[test]
fn load_dir_rejects_a_symlinked_fragment_file() {
    use std::os::unix::fs::symlink;
    let outside = temp_dir();
    let secret = outside.path.join("secret.md");
    fs::write(&secret, "---\nname: leaked\n---\nTOP SECRET").unwrap();

    let dir = temp_dir();
    // A workspace-controlled .iris/fragments/*.md that symlinks to a host file
    // must not be loaded into the prompt.
    symlink(&secret, dir.path.join("evil.md")).unwrap();
    let frags = load_dir(&dir.path, Source::Repo);
    assert!(frags.iter().all(|f| !f.body.contains("TOP SECRET")));
}

#[test]
fn template_fragment_ships_disabled_and_is_never_rendered() {
    // The template materializes into ~/.iris/fragments for discoverability but
    // carries slot 0, so it must stay out of the assembled prompt.
    assert!(
        DEFAULTS
            .iter()
            .any(|d| d.name == "template" && d.slot == Some(0))
    );
    let prompt = build_prompt(
        default_fragments(),
        &built_in_tools(),
        Path::new("/tmp/iris"),
        &[],
        "2026-06-18",
    );
    assert!(!prompt.contains("<template>"));
    assert!(!prompt.contains("This file is a copy-ready template"));
}

#[test]
fn template_is_materialized_to_disk_with_slot_zero() {
    let dir = temp_dir();
    let frag_dir = dir.path.join("fragments");
    materialize_defaults(&frag_dir).unwrap();
    let contents = fs::read_to_string(frag_dir.join("template.md")).unwrap();
    assert!(contents.contains("name: template"));
    assert!(contents.contains("slot: 0"));
    // Re-parsing it yields a disabled fragment (Some(0)).
    let loaded = load_dir(&frag_dir, Source::Global);
    assert!(
        loaded
            .iter()
            .any(|f| f.name == "template" && f.slot == Some(0))
    );
}

#[test]
fn every_shipped_default_has_a_description() {
    assert!(
        DEFAULTS.iter().all(|d| !d.description.trim().is_empty()),
        "each shipped default must document its intent"
    );
}

#[test]
fn materialized_default_files_carry_a_description_kept_out_of_the_body() {
    let dir = temp_dir();
    let frag_dir = dir.path.join("fragments");
    materialize_defaults(&frag_dir).unwrap();

    let identity = fs::read_to_string(frag_dir.join("identity.md")).unwrap();
    assert!(
        identity.contains("description: Who iris is"),
        "materialized frontmatter carries the description"
    );
    // Reloading keeps the description (metadata) out of the rendered body.
    let loaded = load_dir(&frag_dir, Source::Global);
    let id = loaded.iter().find(|f| f.name == "identity").unwrap();
    assert!(!id.body.contains("description:"));
}

#[test]
fn load_dir_reads_only_md_files() {
    let dir = temp_dir();
    fs::write(dir.path.join("a.md"), "---\nname: a\n---\nbody a").unwrap();
    fs::write(dir.path.join("b.txt"), "ignored").unwrap();
    let frags = load_dir(&dir.path, Source::Repo);
    assert_eq!(frags.len(), 1);
    assert_eq!(frags[0].name, "a");
}

#[test]
fn materialize_writes_defaults_then_does_not_overwrite() {
    let dir = temp_dir();
    let frag_dir = dir.path.join("fragments");
    materialize_defaults(&frag_dir).unwrap();
    // Every shipped default lands as a file and re-parses to its name/slot.
    let loaded = load_dir(&frag_dir, Source::Global);
    assert_eq!(loaded.len(), DEFAULTS.len());
    assert!(
        loaded
            .iter()
            .any(|f| f.name == "identity" && f.slot.is_none())
    );
    assert!(
        loaded
            .iter()
            .any(|f| f.name == "mission" && f.slot == Some(1))
    );
    assert!(
        loaded
            .iter()
            .any(|f| f.name == "response_style" && f.slot == Some(2))
    );

    // A user edit survives a second materialize (no overwrite).
    let edited = frag_dir.join("response_style.md");
    fs::write(&edited, "---\nname: response_style\nslot: 1\n---\nMY EDIT").unwrap();
    materialize_defaults(&frag_dir).unwrap();
    assert_eq!(
        fs::read_to_string(&edited).unwrap(),
        "---\nname: response_style\nslot: 1\n---\nMY EDIT"
    );
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
fn assemble_falls_back_to_shipped_defaults_when_dirs_are_empty() {
    // A workspace with no .iris/fragments dir and (typically) no global dir
    // must still produce the full default prompt. Use a temp workspace so the
    // repo dir is absent; the global dir may or may not exist on the dev host,
    // so assert the defaults' content is present either way.
    let ws = temp_dir();
    let prompt = assemble(&ws.path, &built_in_tools());
    assert!(prompt.contains("<identity>"));
    assert!(prompt.contains("You are iris, a coding assistant"));
    assert!(prompt.contains("<available_tools>"));
    assert!(prompt.contains("<tool_use>"));
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
fn dropping_a_repo_fragment_changes_the_prompt_at_its_slot_position() {
    // New behavior: a user-dropped slot:N fragment lands at the expected spot.
    let mut frags = default_fragments();
    frags.push(frag(
        "custom_rule",
        Some(3),
        Source::Repo,
        "always run the linter",
    ));
    let prompt = build_with(frags, &[]);
    assert!(prompt.contains("<custom_rule>"));
    assert!(prompt.contains("always run the linter"));
    // slot 3 sits after response_style (slot 2) and before default_to_action
    // (slot 4); same-slot rule does not apply since slots differ.
    assert!(at(&prompt, "response_style") < at(&prompt, "custom_rule"));
    assert!(at(&prompt, "custom_rule") < at(&prompt, "default_to_action"));
}

#[test]
fn rendering_wraps_each_block_in_its_tag_separated_by_blank_lines() {
    let frags = vec![
        frag("identity", None, Source::Global, "id body"),
        frag("alpha", Some(1), Source::Global, "alpha body"),
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
