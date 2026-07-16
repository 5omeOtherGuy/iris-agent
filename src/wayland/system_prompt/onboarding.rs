//! First-run discovery of user-level agent instructions from peer tools.
//!
//! When neither `~/.iris/AGENTS.md` nor a non-empty shared
//! `~/.agents/AGENTS.md` is active, this module scans known peer-tool home
//! directories (`~/.pi/agent/AGENTS.md`, `~/.claude/CLAUDE.md`, etc.), presents
//! existing instruction files to the user on an interactive TTY, and persists
//! the choice to `~/.iris/AGENTS.md`. A skip creates a zero-byte sentinel so the
//! prompt never recurs.
//!
//! Peer reads use [`super::read_regular_bounded`]. Shared-hub detection uses the
//! same bounded symlink-following policy as prompt assembly. The module never
//! writes outside `~/.iris/`.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use super::{LinkPolicy, MAX_DOC_BYTES, ReadOutcome, read_bounded, read_regular_bounded};

/// Peer-tool candidate paths relative to `$HOME`, in display order.
/// Each entry: `(relative_path, tool_display_name)`.
const PEER_CANDIDATES: &[(&str, &str)] = &[
    (".pi/agent/AGENTS.md", "pi"),
    (".claude/CLAUDE.md", "Claude Code"),
    (".codex/instructions.md", "Codex"),
    (".amp/AGENTS.md", "AMP"),
];

/// The Iris user-level instructions file.
const IRIS_AGENTS_FILENAME: &str = "AGENTS.md";

/// The Iris home subdirectory.
const IRIS_HOME_DIR: &str = ".iris";

/// A discovered peer-tool instruction file.
#[derive(Debug)]
pub(crate) struct PeerDoc {
    /// Human-readable tool name (e.g. "pi", "Claude Code").
    pub(crate) tool: String,
    /// Absolute path to the file.
    pub(crate) path: PathBuf,
    /// File content (bounded, symlink-safe).
    pub(crate) content: String,
}

/// Directory of the machine-local shared agents hub: `$HOME/.agents`. Iris
/// reads its user-level instructions from this hub alongside Iris-specific
/// overrides in `~/.iris`.
const SHARED_AGENTS_DIR: &str = ".agents";

/// Resolve the shared cross-harness AGENTS.md path: `$HOME/.agents/AGENTS.md`.
/// Returns `None` when `HOME` is not set or empty.
pub(crate) fn shared_agents_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(
        Path::new(&home)
            .join(SHARED_AGENTS_DIR)
            .join(IRIS_AGENTS_FILENAME),
    )
}

/// Resolve the Iris user-level AGENTS.md path: `$HOME/.iris/AGENTS.md`.
/// Returns `None` when `HOME` is not set or empty.
pub(crate) fn iris_agents_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    Some(
        Path::new(&home)
            .join(IRIS_HOME_DIR)
            .join(IRIS_AGENTS_FILENAME),
    )
}

/// Return whether first-run copying is still useful for this home directory.
/// A non-empty shared hub already supplies user guidance to Iris, so copying a
/// peer file into the Iris-specific layer would only duplicate instructions.
fn onboarding_needed(home: &Path) -> bool {
    let iris_agents = home.join(IRIS_HOME_DIR).join(IRIS_AGENTS_FILENAME);
    if std::fs::symlink_metadata(iris_agents).is_ok() {
        return false;
    }

    let shared_agents = home.join(SHARED_AGENTS_DIR).join(IRIS_AGENTS_FILENAME);
    !matches!(
        read_bounded(&shared_agents, MAX_DOC_BYTES, LinkPolicy::Follow),
        ReadOutcome::Content(content) if !content.trim().is_empty()
    )
}

/// Scan peer-tool home directories for existing instruction files.
/// Returns only non-empty, non-symlink regular files within `MAX_DOC_BYTES`.
pub(crate) fn discover_peer_docs(home: &Path) -> Vec<PeerDoc> {
    PEER_CANDIDATES
        .iter()
        .filter_map(|(rel, tool)| {
            let path = home.join(rel);
            let content = read_regular_bounded(&path, MAX_DOC_BYTES)?;
            if content.trim().is_empty() {
                return None;
            }
            Some(PeerDoc {
                tool: tool.to_string(),
                path,
                content,
            })
        })
        .collect()
}

/// Result of the interactive onboarding prompt.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OnboardingChoice {
    /// User picked a peer doc (0-indexed into the candidates vec).
    Selected(usize),
    /// User chose to skip onboarding.
    Skipped,
}

/// Present discovered peer docs to the user and return their choice.
/// `reader`/`writer` are abstracted for testing (production: stdin/stderr).
pub(crate) fn prompt_user(
    docs: &[PeerDoc],
    reader: &mut dyn BufRead,
    writer: &mut dyn Write,
) -> OnboardingChoice {
    let _ = writeln!(
        writer,
        "\nNo user-level AGENTS.md found for Iris (~/.iris/AGENTS.md)."
    );
    let _ = writeln!(writer, "Found agent instructions from other tools:\n");
    for (i, doc) in docs.iter().enumerate() {
        let size = doc.content.len();
        let (size_val, unit) = if size >= 1024 {
            (format!("{:.1}", size as f64 / 1024.0), "KB")
        } else {
            (format!("{}", size), "B")
        };
        let _ = writeln!(
            writer,
            "  [{}] {}  ({}, {} {})",
            i + 1,
            doc.path.display(),
            doc.tool,
            size_val,
            unit,
        );
    }
    let _ = write!(
        writer,
        "\nPick a number to copy it as your ~/.iris/AGENTS.md, or [s]kip: "
    );
    let _ = writer.flush();

    let mut input = String::new();
    if reader.read_line(&mut input).is_err() {
        return OnboardingChoice::Skipped;
    }
    let input = input.trim();
    if input.eq_ignore_ascii_case("s") || input.is_empty() {
        return OnboardingChoice::Skipped;
    }
    match input.parse::<usize>() {
        Ok(n) if n >= 1 && n <= docs.len() => OnboardingChoice::Selected(n - 1),
        _ => OnboardingChoice::Skipped,
    }
}

/// Persist the onboarding choice to `~/.iris/AGENTS.md`.
/// - `Selected(i)`: copies the content of `docs[i]` to the file.
/// - `Skipped`: creates a zero-byte sentinel.
///
/// Errors are logged but not fatal -- a failed write simply means the prompt
/// will recur next session.
pub(crate) fn persist_choice(
    iris_agents: &Path,
    docs: &[PeerDoc],
    choice: &OnboardingChoice,
    writer: &mut dyn Write,
) {
    if let Some(parent) = iris_agents.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        let _ = writeln!(
            writer,
            "warning: could not create {}: {e}",
            parent.display()
        );
        return;
    }
    // Refuse to write through a symlink: a broken `~/.iris/AGENTS.md -> /outside`
    // would let persist_choice place content outside `~/.iris/`.
    if std::fs::symlink_metadata(iris_agents).is_ok_and(|m| m.is_symlink()) {
        let _ = writeln!(
            writer,
            "warning: {} is a symlink, skipping write",
            iris_agents.display()
        );
        return;
    }
    match choice {
        OnboardingChoice::Selected(idx) => {
            let doc = &docs[*idx];
            match std::fs::write(iris_agents, &doc.content) {
                Ok(()) => {
                    let _ = writeln!(
                        writer,
                        "Copied {} instructions to {}",
                        doc.tool,
                        iris_agents.display(),
                    );
                }
                Err(e) => {
                    let _ = writeln!(
                        writer,
                        "warning: could not write {}: {e}",
                        iris_agents.display(),
                    );
                }
            }
        }
        OnboardingChoice::Skipped => {
            // Zero-byte sentinel: the file existing (even empty) prevents
            // re-prompting. The discover_project_docs walk skips empty docs, so
            // a sentinel adds nothing to the system prompt.
            if let Err(e) = std::fs::write(iris_agents, b"") {
                let _ = writeln!(
                    writer,
                    "warning: could not write sentinel {}: {e}",
                    iris_agents.display(),
                );
            }
        }
    }
}

/// Top-level onboarding entry point. Call before `assemble()` in every startup
/// path (fresh, resume, continue). Only acts when:
/// 1. neither `~/.iris/AGENTS.md` nor an active shared hub supplies user rules,
/// 2. at least one peer doc is found,
/// 3. both stdin and stderr are interactive TTYs.
///
/// Non-interactive modes (--print, piped stdin, redirected stderr) never see a
/// prompt.
pub(crate) fn maybe_onboard() {
    let home = match std::env::var("HOME").ok().filter(|h| !h.is_empty()) {
        Some(h) => PathBuf::from(h),
        None => return,
    };
    if !onboarding_needed(&home) {
        return;
    }
    let docs = discover_peer_docs(&home);
    if docs.is_empty() {
        return;
    }
    // Only prompt when both stdin (for reading input) and stderr (for showing
    // the prompt) are interactive terminals. Checking only stdin would block
    // invisibly when stderr is redirected (`iris 2>log`).
    let stdin = std::io::stdin();
    if !stdin.is_terminal() || !std::io::stderr().is_terminal() {
        return;
    }
    let mut reader = stdin.lock();
    let mut writer = std::io::stderr();
    let choice = prompt_user(&docs, &mut reader, &mut writer);
    if let Some(iris_agents) = iris_agents_path() {
        persist_choice(&iris_agents, &docs, &choice, &mut writer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("iris-onboard-test-{nanos}-{seq}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn discover_finds_existing_peer_docs() {
        let home = test_dir();
        let pi_dir = home.join(".pi/agent");
        fs::create_dir_all(&pi_dir).unwrap();
        fs::write(pi_dir.join("AGENTS.md"), "pi rules").unwrap();

        let claude_dir = home.join(".claude");
        fs::create_dir_all(&claude_dir).unwrap();
        fs::write(claude_dir.join("CLAUDE.md"), "claude rules").unwrap();

        let docs = discover_peer_docs(&home);
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].tool, "pi");
        assert_eq!(docs[0].content, "pi rules");
        assert_eq!(docs[1].tool, "Claude Code");
        assert_eq!(docs[1].content, "claude rules");

        cleanup(&home);
    }

    #[test]
    fn discover_returns_empty_when_no_peers_exist() {
        let home = test_dir();
        let docs = discover_peer_docs(&home);
        assert!(docs.is_empty());
        cleanup(&home);
    }

    #[test]
    fn discover_skips_empty_peer_docs() {
        let home = test_dir();
        let pi_dir = home.join(".pi/agent");
        fs::create_dir_all(&pi_dir).unwrap();
        fs::write(pi_dir.join("AGENTS.md"), "  \n\t\n").unwrap();

        let docs = discover_peer_docs(&home);
        assert!(docs.is_empty());

        cleanup(&home);
    }

    #[test]
    fn active_shared_hub_suppresses_redundant_iris_onboarding() {
        let home = test_dir();
        let hub_dir = home.join(".agents");
        fs::create_dir_all(&hub_dir).unwrap();
        fs::write(hub_dir.join("AGENTS.md"), "shared rules").unwrap();
        let pi_dir = home.join(".pi/agent");
        fs::create_dir_all(&pi_dir).unwrap();
        fs::write(pi_dir.join("AGENTS.md"), "duplicate candidate").unwrap();

        assert!(!onboarding_needed(&home));
        assert!(!home.join(".iris/AGENTS.md").exists());

        cleanup(&home);
    }

    #[cfg(unix)]
    #[test]
    fn discover_rejects_symlinked_peer_doc() {
        use std::os::unix::fs::symlink;
        let home = test_dir();
        let secret_dir = test_dir();
        let secret = secret_dir.join("secret.txt");
        fs::write(&secret, "TOP SECRET").unwrap();

        let pi_dir = home.join(".pi/agent");
        fs::create_dir_all(&pi_dir).unwrap();
        symlink(&secret, pi_dir.join("AGENTS.md")).unwrap();

        let docs = discover_peer_docs(&home);
        assert!(
            docs.iter().all(|d| !d.content.contains("TOP SECRET")),
            "symlinked peer doc must not be read"
        );

        cleanup(&home);
        cleanup(&secret_dir);
    }

    #[test]
    fn prompt_user_selects_a_doc() {
        let docs = vec![
            PeerDoc {
                tool: "pi".into(),
                path: PathBuf::from("/home/x/.pi/agent/AGENTS.md"),
                content: "pi rules".into(),
            },
            PeerDoc {
                tool: "Claude Code".into(),
                path: PathBuf::from("/home/x/.claude/CLAUDE.md"),
                content: "claude rules".into(),
            },
        ];
        let mut input = Cursor::new(b"1\n".to_vec());
        let mut output = Vec::new();
        let choice = prompt_user(&docs, &mut input, &mut output);
        assert_eq!(choice, OnboardingChoice::Selected(0));
    }

    #[test]
    fn prompt_user_selects_second_doc() {
        let docs = vec![
            PeerDoc {
                tool: "pi".into(),
                path: PathBuf::from("/home/x/.pi/agent/AGENTS.md"),
                content: "pi rules".into(),
            },
            PeerDoc {
                tool: "Claude Code".into(),
                path: PathBuf::from("/home/x/.claude/CLAUDE.md"),
                content: "claude rules".into(),
            },
        ];
        let mut input = Cursor::new(b"2\n".to_vec());
        let mut output = Vec::new();
        let choice = prompt_user(&docs, &mut input, &mut output);
        assert_eq!(choice, OnboardingChoice::Selected(1));
    }

    #[test]
    fn prompt_user_skips_on_s() {
        let docs = vec![PeerDoc {
            tool: "pi".into(),
            path: PathBuf::from("/home/x/.pi/agent/AGENTS.md"),
            content: "pi rules".into(),
        }];
        let mut input = Cursor::new(b"s\n".to_vec());
        let mut output = Vec::new();
        let choice = prompt_user(&docs, &mut input, &mut output);
        assert_eq!(choice, OnboardingChoice::Skipped);
    }

    #[test]
    fn prompt_user_skips_on_empty_input() {
        let docs = vec![PeerDoc {
            tool: "pi".into(),
            path: PathBuf::from("/home/x/.pi/agent/AGENTS.md"),
            content: "pi rules".into(),
        }];
        let mut input = Cursor::new(b"\n".to_vec());
        let mut output = Vec::new();
        let choice = prompt_user(&docs, &mut input, &mut output);
        assert_eq!(choice, OnboardingChoice::Skipped);
    }

    #[test]
    fn prompt_user_skips_on_invalid_number() {
        let docs = vec![PeerDoc {
            tool: "pi".into(),
            path: PathBuf::from("/home/x/.pi/agent/AGENTS.md"),
            content: "pi rules".into(),
        }];
        let mut input = Cursor::new(b"99\n".to_vec());
        let mut output = Vec::new();
        let choice = prompt_user(&docs, &mut input, &mut output);
        assert_eq!(choice, OnboardingChoice::Skipped);
    }

    #[test]
    fn persist_selected_copies_content() {
        let dir = test_dir();
        let iris_agents = dir.join("AGENTS.md");
        let docs = vec![PeerDoc {
            tool: "pi".into(),
            path: PathBuf::from("/home/x/.pi/agent/AGENTS.md"),
            content: "pi rules content".into(),
        }];
        let mut output = Vec::new();
        persist_choice(
            &iris_agents,
            &docs,
            &OnboardingChoice::Selected(0),
            &mut output,
        );

        assert!(iris_agents.exists());
        assert_eq!(
            fs::read_to_string(&iris_agents).unwrap(),
            "pi rules content"
        );
        let msg = String::from_utf8(output).unwrap();
        assert!(msg.contains("Copied pi instructions"));

        cleanup(&dir);
    }

    #[test]
    fn persist_skip_creates_sentinel() {
        let dir = test_dir();
        let iris_agents = dir.join("AGENTS.md");
        let docs: Vec<PeerDoc> = vec![];
        let mut output = Vec::new();
        persist_choice(&iris_agents, &docs, &OnboardingChoice::Skipped, &mut output);

        assert!(iris_agents.exists());
        assert_eq!(fs::read_to_string(&iris_agents).unwrap(), "");

        cleanup(&dir);
    }

    #[test]
    fn persist_creates_parent_directories() {
        let dir = test_dir();
        let iris_agents = dir.join("sub/dir/AGENTS.md");
        let docs = vec![PeerDoc {
            tool: "pi".into(),
            path: PathBuf::from("/home/x/.pi/agent/AGENTS.md"),
            content: "content".into(),
        }];
        let mut output = Vec::new();
        persist_choice(
            &iris_agents,
            &docs,
            &OnboardingChoice::Selected(0),
            &mut output,
        );

        assert!(iris_agents.exists());
        assert_eq!(fs::read_to_string(&iris_agents).unwrap(), "content");

        cleanup(&dir);
    }

    #[test]
    fn idempotent_second_run_no_prompt() {
        // Once the sentinel or real file exists, onboarding is not needed.
        let dir = test_dir();
        let iris_agents = dir.join(".iris/AGENTS.md");
        fs::create_dir_all(iris_agents.parent().unwrap()).unwrap();
        fs::write(&iris_agents, "").unwrap();

        assert!(iris_agents.exists());
        assert!(!onboarding_needed(&dir));

        cleanup(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn persist_refuses_to_write_through_symlink() {
        use std::os::unix::fs::symlink;
        let dir = test_dir();
        let outside = dir.join("outside.txt");
        let iris_agents = dir.join("AGENTS.md");
        symlink(&outside, &iris_agents).unwrap();

        let docs = vec![PeerDoc {
            tool: "pi".into(),
            path: PathBuf::from("/home/x/.pi/agent/AGENTS.md"),
            content: "should not land outside".into(),
        }];
        let mut output = Vec::new();
        persist_choice(
            &iris_agents,
            &docs,
            &OnboardingChoice::Selected(0),
            &mut output,
        );

        // The symlink target must not be created.
        assert!(
            !outside.exists(),
            "persist_choice must not write through a symlink"
        );
        let msg = String::from_utf8(output).unwrap();
        assert!(msg.contains("is a symlink"));

        cleanup(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn iris_agents_exists_detects_broken_symlink() {
        use std::os::unix::fs::symlink;
        let dir = test_dir();
        let iris_dir = dir.join(".iris");
        fs::create_dir_all(&iris_dir).unwrap();
        // Broken symlink: target does not exist.
        symlink("/nonexistent/target", iris_dir.join("AGENTS.md")).unwrap();

        // symlink_metadata detects it; .exists() would return false.
        assert!(std::fs::symlink_metadata(iris_dir.join("AGENTS.md")).is_ok());
        assert!(!iris_dir.join("AGENTS.md").exists());

        cleanup(&dir);
    }
}
