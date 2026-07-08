use std::path::Path;

pub(crate) fn workspace_path(workspace: &Path, path: &Path) -> String {
    let display = path.strip_prefix(workspace).unwrap_or(path);
    escape_path(display)
}

pub(crate) fn workspace_path_if_inside(workspace: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(workspace).ok().map(escape_path)
}

fn escape_path(path: &Path) -> String {
    let mut escaped = String::new();
    for ch in path.to_string_lossy().chars() {
        if ch == '\\' || ch.is_control() {
            escaped.extend(ch.escape_default());
        } else {
            escaped.push(ch);
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_path_escapes_terminal_controls() {
        let workspace = Path::new("/repo");
        let path = Path::new("/repo/a\n\u{1b}[31m.txt");

        assert_eq!(
            workspace_path(workspace, path),
            "a\\n\\u{1b}[31m.txt".to_string()
        );
    }

    #[test]
    fn workspace_path_preserves_printable_unicode() {
        let workspace = Path::new("/repo");
        let path = Path::new("/repo/cafe\u{301}.txt");

        assert_eq!(workspace_path(workspace, path), "cafe\u{301}.txt");
    }
}
