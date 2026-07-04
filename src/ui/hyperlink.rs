//! Spans-first OSC 8 hyperlinks (ADR-0033, issue #325).
//!
//! Link targets travel as structured span metadata, never as escape bytes woven
//! through visible strings. The transport is a zero-width APC (Application
//! Program Command) marker pair -- the same contract Iris already uses for the
//! focused-cursor position ([`crate::ui::terminal_surface::CURSOR_MARKER`]):
//!
//! * an OPEN marker span carries the target URI: `ESC _ iris:link:<uri> BEL`
//! * a CLOSE marker span ends the region: `ESC _ iris:endlink BEL`
//!
//! Between the two, the visible label spans render exactly as they would
//! without a link, so `--plain` output and pager text stay byte-identical. The
//! markers are APC strings every terminal ignores, and Iris never writes them
//! to the terminal verbatim:
//!
//! * **Inline mode** -- [`crate::ui::terminal_surface::render_line`] detects the
//!   markers at byte-serialization time and emits the real `ESC ] 8 ; ; <uri>
//!   ST ... ESC ] 8 ; ; ST` pair around the label. The wrap layer keeps each
//!   physical row's marker pair complete (re-opening a link that spans rows), so
//!   the OSC 8 pair is line-atomic and survives the diff/replay model.
//! * **Pager mode** -- ratatui's `Buffer` cannot carry OSC 8, so
//!   [`extract_and_strip`] pulls the markers out of the composed frame (leaving
//!   clean cells) and returns the visible column regions each link covers.
//!   [`region_at`] then resolves a mouse click to a target for the
//!   `open_in_browser`/notice seam.
//!
//! The no-escapes-in-width-math invariant holds because the markers are their
//! own zero-width spans: the wrap and width helpers skip them ([`is_marker`]),
//! and [`crate::ui::textengine::visible_width`] already strips APC, so a marked
//! span measures identically to its bare visible text.
//!
//! # Trust boundary (security)
//!
//! Markers are trusted only when Iris itself constructs them here, from a URI
//! that has passed [`sanitized_link_uri`]. Untrusted model text (markdown
//! source, tool output) can contain the literal marker bytes and would
//! otherwise be re-interpreted as a genuine link -- marker forgery leading to
//! terminal escape injection through [`osc8_open`]. Two defenses hold the line:
//!
//! * Every rendering boundary that ingests untrusted text strips any
//!   pre-existing Iris markers first ([`strip_foreign_markers`] for markdown
//!   source; [`crate::ui::textengine::clean_text`] already drops APC on the
//!   tool-output span path). Only Iris-built markers survive downstream.
//! * Web links are created only from a [`sanitized_link_uri`]-approved URI, and
//!   [`crate::ui::terminal_surface::render_line`] re-checks that same allowlist
//!   before emitting any OSC 8 bytes. An unsanitized URI never becomes an
//!   escape sequence.
//!
//! Workspace `file:line` references are a *separate* marker kind
//! (`iris:fileref:`, see [`fileref_open_span`]): they are deliberately outside
//! the web-scheme allowlist, so inline mode strips them (never emits OSC 8 that
//! a terminal would open natively) while the pager still resolves a click to the
//! `link: <target>` notice.

use std::borrow::Cow;
use std::path::Path;

use ratatui::text::Span;

use crate::ui::textengine::display_width;

/// Upper bound on a link URI length (bytes). Caps the per-row memory a single
/// marker can pin and bounds the OSC 8 sequence a terminal must parse.
const MAX_URI_LEN: usize = 2048;

/// The web-link schemes Iris is willing to emit as clickable OSC 8. `file:` is
/// intentionally excluded (see the file-ref marker kind) so a model-supplied
/// `file://` / `javascript:` / `data:` destination never becomes a hyperlink.
const ALLOWED_SCHEMES: [&str; 3] = ["https://", "http://", "mailto:"];

/// The single validation choke point for every link URI Iris turns into a
/// marker or an OSC 8 escape. Returns the URI unchanged when it is safe, else
/// `None`. Rejects:
///
/// * any C0/C1 control (incl. ESC/BEL) or DEL, and any whitespace -- these break
///   out of the OSC 8 sequence and enable arbitrary escape injection;
/// * anything longer than [`MAX_URI_LEN`];
/// * any scheme outside [`ALLOWED_SCHEMES`].
///
/// No web-link marker is ever constructed from a URI this function rejects.
pub(crate) fn sanitized_link_uri(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > MAX_URI_LEN {
        return None;
    }
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let lower = raw.to_ascii_lowercase();
    if !ALLOWED_SCHEMES
        .iter()
        .any(|scheme| lower.starts_with(scheme))
    {
        return None;
    }
    Some(raw.to_string())
}

/// APC namespace prefix shared by every Iris zero-width marker (link, file-ref,
/// close, and the focused-cursor marker). Used to strip forged markers from
/// untrusted input at the rendering boundary.
const IRIS_APC_NS: &str = "\x1b_iris:";

/// Remove any pre-existing Iris control markers -- link / file-ref / close APC
/// strings and the focused-cursor marker -- from untrusted `text`. Model
/// markdown or tool output that embeds these literal bytes would otherwise be
/// re-interpreted by [`crate::ui::terminal_surface::render_line`] as genuine
/// markers (forgery -> escape injection). Stripping at the rendering boundary
/// guarantees the only markers downstream are the ones Iris constructs itself,
/// after this strip. The visible text is preserved verbatim.
pub(crate) fn strip_foreign_markers(text: &str) -> Cow<'_, str> {
    if !text.contains(IRIS_APC_NS) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find(IRIS_APC_NS) {
        out.push_str(&rest[..idx]);
        // Skip the whole APC string: from its ESC introducer to the terminator
        // (BEL, or 7-bit ST `ESC \`) inclusive. A malformed marker with no
        // terminator is dropped to end-of-string.
        let after = &rest[idx..];
        let bytes = after.as_bytes();
        let mut end = after.len();
        // Start at 1 to skip the marker's own leading ESC.
        let mut k = 1;
        while k < bytes.len() {
            if bytes[k] == 0x07 {
                end = k + 1;
                break;
            }
            if bytes[k] == 0x1b && bytes.get(k + 1) == Some(&b'\\') {
                end = k + 2;
                break;
            }
            k += 1;
        }
        rest = &after[end..];
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// APC introducer + Iris link namespace for the OPEN marker. The URI follows,
/// terminated by BEL ([`APC_TERM`]).
const OPEN_PREFIX: &str = "\x1b_iris:link:";
/// APC introducer + Iris namespace for a workspace `file:line` reference OPEN
/// marker. A distinct kind from [`OPEN_PREFIX`]: file refs are outside the
/// web-scheme allowlist, so inline serialization strips them (never emits OSC 8)
/// while the pager still resolves a click to the file-ref notice.
const FILEREF_PREFIX: &str = "\x1b_iris:fileref:";
/// BEL terminator for the APC string (mirrors `CURSOR_MARKER`).
const APC_TERM: char = '\x07';
/// The CLOSE marker span content: a complete, argument-free APC string. Shared
/// by both the web-link and file-ref OPEN markers.
pub(crate) const CLOSE_MARKER: &str = "\x1b_iris:endlink\x07";

/// OSC 8 hyperlink close (empty params + empty URI), ST-terminated. The ST
/// (`ESC \`) form is used over BEL so terminal multiplexers forward it cleanly.
pub(crate) const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";

/// Build the OPEN marker string for `uri`.
pub(crate) fn open_marker(uri: &str) -> String {
    format!("{OPEN_PREFIX}{uri}{APC_TERM}")
}

/// A zero-width OPEN marker span carrying `uri`.
pub(crate) fn open_span(uri: &str) -> Span<'static> {
    Span::raw(open_marker(uri))
}

/// A zero-width CLOSE marker span.
pub(crate) fn close_span() -> Span<'static> {
    Span::raw(CLOSE_MARKER)
}

/// The URI carried by a web-link OPEN marker span content, if `content` is one.
/// Web links are the only marker kind [`crate::ui::terminal_surface::render_line`]
/// turns into OSC 8 escapes.
pub(crate) fn marker_uri(content: &str) -> Option<&str> {
    content
        .strip_prefix(OPEN_PREFIX)
        .and_then(|rest| rest.strip_suffix(APC_TERM))
}

/// Build the OPEN marker string for a workspace file reference `uri`.
fn fileref_open_marker(uri: &str) -> String {
    format!("{FILEREF_PREFIX}{uri}{APC_TERM}")
}

/// A zero-width file-reference OPEN marker span carrying `uri`.
pub(crate) fn fileref_open_span(uri: &str) -> Span<'static> {
    Span::raw(fileref_open_marker(uri))
}

/// The target carried by a file-reference OPEN marker span content, if any.
pub(crate) fn fileref_uri(content: &str) -> Option<&str> {
    content
        .strip_prefix(FILEREF_PREFIX)
        .and_then(|rest| rest.strip_suffix(APC_TERM))
}

/// The target carried by *any* OPEN marker (web link or file ref). Used by the
/// pager hit-testing and the wrap layer, which treat both kinds uniformly for
/// region extraction and per-row re-opening.
pub(crate) fn open_marker_uri(content: &str) -> Option<&str> {
    marker_uri(content).or_else(|| fileref_uri(content))
}

/// Whether `content` is the CLOSE marker.
pub(crate) fn is_close(content: &str) -> bool {
    content == CLOSE_MARKER
}

/// Whether `content` is either a link marker (OPEN or CLOSE). Used by the wrap /
/// width helpers to skip markers so they never enter width or text accounting.
pub(crate) fn is_marker(content: &str) -> bool {
    is_close(content) || open_marker_uri(content).is_some()
}

/// The real OSC 8 open sequence for `uri` (empty params), ST-terminated.
pub(crate) fn osc8_open(uri: &str) -> String {
    format!("\x1b]8;;{uri}\x1b\\")
}

/// Wrap `visible` spans in an OPEN/CLOSE marker pair targeting `uri`. A test
/// helper today (production callers build the pair inline around their label);
/// kept here so the marker contract has one constructor.
#[cfg(test)]
pub(crate) fn link_spans(uri: &str, visible: Vec<Span<'static>>) -> Vec<Span<'static>> {
    let mut out = Vec::with_capacity(visible.len() + 2);
    out.push(open_span(uri));
    out.extend(visible);
    out.push(close_span());
    out
}

// ---------------------------------------------------------------------------
// Pager hit-testing: strip markers, keep the visible regions.
// ---------------------------------------------------------------------------

/// A resolved clickable link region in a composed pager frame: the target
/// `uri`, the frame `row`, and the half-open visible column range
/// `[start_col, end_col)` the label occupies after markers are stripped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkRegion {
    pub(crate) row: usize,
    pub(crate) start_col: usize,
    pub(crate) end_col: usize,
    pub(crate) uri: String,
}

/// Strip every link marker span from `lines` in place and return the visible
/// column regions the links covered. The pager calls this before writing to the
/// ratatui `Buffer` (which cannot carry OSC 8), so the cells hold clean text and
/// the returned regions drive mouse hit-testing. Mirrors the marker-strip half
/// of [`crate::ui::tui::component::take_marker_position`].
pub(crate) fn extract_and_strip_lines(
    lines: &mut [ratatui::text::Line<'static>],
) -> Vec<LinkRegion> {
    let mut regions = Vec::new();
    for (row, line) in lines.iter_mut().enumerate() {
        let mut kept: Vec<Span<'static>> = Vec::with_capacity(line.spans.len());
        let mut col = 0usize;
        let mut open: Option<(usize, String)> = None;
        for span in std::mem::take(&mut line.spans) {
            let content = span.content.as_ref();
            if let Some(uri) = open_marker_uri(content) {
                open = Some((col, uri.to_string()));
                continue;
            }
            if is_close(content) {
                if let Some((start, uri)) = open.take()
                    && col > start
                {
                    regions.push(LinkRegion {
                        row,
                        start_col: start,
                        end_col: col,
                        uri,
                    });
                }
                continue;
            }
            col += display_width(content);
            kept.push(span);
        }
        // A link left open at the line end (e.g. a wrapped row that re-opened
        // but never re-closed) still covers to the row's end.
        if let Some((start, uri)) = open.take()
            && col > start
        {
            regions.push(LinkRegion {
                row,
                start_col: start,
                end_col: col,
                uri,
            });
        }
        line.spans = kept;
    }
    regions
}

/// Resolve a `(row, col)` click against extracted regions.
pub(crate) fn region_at(regions: &[LinkRegion], row: usize, col: usize) -> Option<&LinkRegion> {
    regions
        .iter()
        .find(|region| region.row == row && col >= region.start_col && col < region.end_col)
}

/// Whether a resolved target is a web URL (opened in the browser) rather than a
/// workspace file reference (surfaced as a notice). Delegates to the same
/// validation choke point that gated marker creation and OSC emission, so pager
/// click classification can never diverge from it (e.g. case-insensitive
/// schemes like `HTTPS://` that `sanitized_link_uri` accepts).
pub(crate) fn is_web_url(uri: &str) -> bool {
    sanitized_link_uri(uri).is_some()
}

// ---------------------------------------------------------------------------
// Conservative file:line reference extraction (tool output panels).
// ---------------------------------------------------------------------------

/// A detected `path:line` reference inside a plain string: the byte range in the
/// source, the workspace-relative path, and the 1-based line number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileRef {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) path: String,
    pub(crate) line: u32,
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-')
}

/// The bytes a workspace path may *begin* with. Deliberately narrower than
/// [`is_path_byte`]: `-` is a valid interior byte (`my-file.rs`) but never a
/// leading one, so unified-diff body lines like `-src/main.rs:12` are not
/// mistaken for a file reference.
fn is_path_start_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/')
}

/// A path is link-worthy only if it is an obvious workspace-relative file: not
/// absolute, no `..` traversal, has an extension-like dot in the final segment,
/// and no empty segments. Deliberately conservative -- a false positive turns
/// ordinary text into a misleading link.
fn is_workspace_file(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.contains("..") {
        return false;
    }
    let segments: Vec<&str> = path.split('/').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return false;
    }
    // The final segment must look like a filename: `name.ext`.
    match segments.last() {
        Some(last) => last
            .rsplit_once('.')
            .is_some_and(|(stem, ext)| !stem.is_empty() && !ext.is_empty()),
        None => false,
    }
}

/// Find conservative `path:line` references in `text`. Only obvious
/// workspace-relative file references (see [`is_workspace_file`]) followed by a
/// colon and a line number are returned; anything ambiguous is skipped.
pub(crate) fn find_file_refs(text: &str) -> Vec<FileRef> {
    let bytes = text.as_bytes();
    let mut refs = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        // A path run must start on a boundary (not mid-token) with a valid
        // leading byte, and must not be glued to a diff `+`/`-` prefix -- that
        // is added/removed line content, not a real path. A whitespace-separated
        // ref after a diff marker (e.g. `- src/main.rs:12`) still linkifies.
        if !is_path_start_byte(bytes[i]) || (i > 0 && is_path_byte(bytes[i - 1])) {
            i += 1;
            continue;
        }
        if i > 0 && matches!(bytes[i - 1], b'+' | b'-') {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && is_path_byte(bytes[i]) {
            i += 1;
        }
        let path = &text[start..i];
        // Require `:` then a digit run.
        if i >= bytes.len() || bytes[i] != b':' {
            continue;
        }
        let digits_start = i + 1;
        let mut j = digits_start;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j == digits_start {
            continue;
        }
        // Reject `file:1:2` / `file:1.2` style trailing ambiguity.
        if j < bytes.len() && matches!(bytes[j], b':' | b'.') {
            i = j;
            continue;
        }
        if !is_workspace_file(path) {
            continue;
        }
        let Ok(line) = text[digits_start..j].parse::<u32>() else {
            continue;
        };
        refs.push(FileRef {
            start,
            end: j,
            path: path.to_string(),
            line,
        });
        i = j;
    }
    refs
}

/// The `file://` link target for a workspace file reference resolved against
/// `root`. Encodes the 1-based line as an `#L<n>` fragment. Web-URL detection
/// ([`is_web_url`]) returns false for this, so the click seam surfaces it as a
/// notice rather than launching a browser.
pub(crate) fn file_ref_uri(root: &Path, path: &str, line: u32) -> String {
    let abs = root.join(path);
    format!("file://{}#L{line}", abs.display())
}

/// Split a plain `text` span (with `style`) into visible spans, wrapping any
/// conservative `path:line` reference in an OPEN/CLOSE marker pair whose target
/// is resolved against `root`. Non-reference text is preserved verbatim, so the
/// visible output is byte-identical to `text`.
pub(crate) fn linkify_file_refs(
    text: &str,
    style: ratatui::style::Style,
    root: &Path,
) -> Vec<Span<'static>> {
    let refs = find_file_refs(text);
    if refs.is_empty() {
        return vec![Span::styled(text.to_string(), style)];
    }
    let mut spans = Vec::new();
    let mut cursor = 0usize;
    for file_ref in refs {
        if file_ref.start > cursor {
            spans.push(Span::styled(
                text[cursor..file_ref.start].to_string(),
                style,
            ));
        }
        let uri = file_ref_uri(root, &file_ref.path, file_ref.line);
        spans.push(fileref_open_span(&uri));
        spans.push(Span::styled(
            text[file_ref.start..file_ref.end].to_string(),
            style,
        ));
        spans.push(close_span());
        cursor = file_ref.end;
    }
    if cursor < text.len() {
        spans.push(Span::styled(text[cursor..].to_string(), style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Style;
    use ratatui::text::Line;

    #[test]
    fn marker_round_trip_open_and_close() {
        let open = open_marker("https://example.com/docs");
        assert_eq!(marker_uri(&open), Some("https://example.com/docs"));
        assert!(!is_close(&open));
        assert!(is_marker(&open));
        assert!(is_close(CLOSE_MARKER));
        assert!(is_marker(CLOSE_MARKER));
        assert_eq!(marker_uri(CLOSE_MARKER), None);
        assert!(!is_marker("plain text"));
        assert_eq!(marker_uri("plain"), None);
    }

    #[test]
    fn osc8_emission_is_a_complete_pair() {
        assert_eq!(osc8_open("https://x.dev"), "\x1b]8;;https://x.dev\x1b\\");
        assert_eq!(OSC8_CLOSE, "\x1b]8;;\x1b\\");
    }

    #[test]
    fn extract_and_strip_returns_regions_and_cleans_spans() {
        let mut lines = vec![Line::from(link_spans(
            "https://x.dev",
            vec![Span::raw("click")],
        ))];
        // Prefix the label with plain text to prove column math.
        lines[0].spans.insert(0, Span::raw("go "));
        let regions = extract_and_strip_lines(&mut lines);
        assert_eq!(
            regions,
            vec![LinkRegion {
                row: 0,
                start_col: 3,
                end_col: 8,
                uri: "https://x.dev".to_string(),
            }]
        );
        // Markers are gone; visible text preserved.
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "go click");
        assert!(!lines[0].spans.iter().any(|s| is_marker(s.content.as_ref())));
    }

    #[test]
    fn region_at_resolves_click_columns() {
        let regions = vec![LinkRegion {
            row: 2,
            start_col: 3,
            end_col: 8,
            uri: "u".to_string(),
        }];
        assert!(region_at(&regions, 2, 2).is_none());
        assert_eq!(region_at(&regions, 2, 3).map(|r| r.uri.as_str()), Some("u"));
        assert_eq!(region_at(&regions, 2, 7).map(|r| r.uri.as_str()), Some("u"));
        assert!(region_at(&regions, 2, 8).is_none(), "end is exclusive");
        assert!(region_at(&regions, 1, 5).is_none(), "wrong row");
    }

    #[test]
    fn is_web_url_classification() {
        assert!(is_web_url("https://x.dev"));
        assert!(is_web_url("http://x.dev"));
        assert!(is_web_url("mailto:a@b.c"));
        // Case-insensitive schemes classify like sanitized_link_uri accepts them
        // (reviewer finding: pager click must match creation/render validation).
        assert!(is_web_url("HTTPS://x.dev"));
        assert!(is_web_url("hTTp://x.dev"));
        assert!(!is_web_url("file:///repo/src/main.rs#L10"));
        assert!(!is_web_url("FILE:///repo/src/main.rs"));
        assert!(!is_web_url("ftp://x"));
        assert!(!is_web_url("//x.dev"));
    }

    #[test]
    fn find_file_refs_positive_cases() {
        let refs = find_file_refs("see src/ui/hyperlink.rs:42 for details");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "src/ui/hyperlink.rs");
        assert_eq!(refs[0].line, 42);
        let slice = &"see src/ui/hyperlink.rs:42 for details"[refs[0].start..refs[0].end];
        assert_eq!(slice, "src/ui/hyperlink.rs:42");

        // Bare filename with extension is accepted.
        let refs = find_file_refs("main.rs:1");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].path, "main.rs");
        assert_eq!(refs[0].line, 1);
    }

    #[test]
    fn find_file_refs_negative_cases() {
        // No extension -> not linkified (avoids matching `foo:12`).
        assert!(find_file_refs("target:12").is_empty());
        // Absolute path -> skipped (outside the workspace-relative contract).
        assert!(find_file_refs("/etc/passwd:1").is_empty());
        // Traversal -> skipped.
        assert!(find_file_refs("../secret.rs:1").is_empty());
        // No line number -> skipped.
        assert!(find_file_refs("src/main.rs").is_empty());
        // A time-like `12:34` is not a file ref.
        assert!(find_file_refs("elapsed 12:34 done").is_empty());
        // Ambiguous multi-colon (line:col:extra) is skipped.
        assert!(find_file_refs("src/main.rs:10:5").is_empty());
    }

    #[test]
    fn linkify_file_refs_preserves_visible_text() {
        let root = Path::new("/repo");
        let spans = linkify_file_refs("at src/main.rs:7 now", Style::default(), root);
        let visible: String = spans
            .iter()
            .filter(|s| !is_marker(s.content.as_ref()))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(visible, "at src/main.rs:7 now");
        // The reference is wrapped in a *file-ref* marker pair (a distinct kind
        // from web links) with a resolved file:// uri. It is NOT a web-link
        // marker, so inline mode never emits OSC 8 for it (finding 3).
        let uri = spans
            .iter()
            .find_map(|s| fileref_uri(s.content.as_ref()))
            .expect("file-ref open marker present");
        assert_eq!(uri, "file:///repo/src/main.rs#L7");
        assert!(
            !spans
                .iter()
                .any(|s| marker_uri(s.content.as_ref()).is_some()),
            "file refs must not be web-link markers"
        );
        assert!(spans.iter().any(|s| is_close(s.content.as_ref())));
    }

    #[test]
    fn linkify_without_refs_is_a_single_plain_span() {
        let spans = linkify_file_refs("no refs here", Style::default(), Path::new("/repo"));
        assert_eq!(spans.len(), 1);
        assert!(!is_marker(spans[0].content.as_ref()));
    }

    // --- Finding 1: sanitized_link_uri choke point --------------------------

    #[test]
    fn sanitized_link_uri_rejects_control_and_escape_injection() {
        // Embedded ESC/OSC/BEL that would break out of the OSC 8 sequence.
        assert_eq!(sanitized_link_uri("https://x.dev/\x1b]0;pwned\x07"), None);
        assert_eq!(sanitized_link_uri("https://x.dev/\x07"), None);
        assert_eq!(sanitized_link_uri("https://x.dev/\nrm"), None);
        // DEL and a raw space are rejected too.
        assert_eq!(sanitized_link_uri("https://x.dev/\x7f"), None);
        assert_eq!(sanitized_link_uri("https://x.dev/a b"), None);
    }

    #[test]
    fn sanitized_link_uri_enforces_scheme_allowlist() {
        assert_eq!(sanitized_link_uri("javascript:alert(1)"), None);
        assert_eq!(sanitized_link_uri("file:///etc/passwd"), None);
        assert_eq!(sanitized_link_uri("data:text/html,<script>"), None);
        assert_eq!(sanitized_link_uri("ftp://x/"), None);
        // Clean web/mail URIs pass through unchanged.
        assert_eq!(
            sanitized_link_uri("https://example.com/docs").as_deref(),
            Some("https://example.com/docs")
        );
        assert_eq!(
            sanitized_link_uri("HTTP://Example.com").as_deref(),
            Some("HTTP://Example.com"),
            "scheme match is case-insensitive; the URI is preserved verbatim"
        );
        assert_eq!(
            sanitized_link_uri("mailto:a@b.c").as_deref(),
            Some("mailto:a@b.c")
        );
    }

    #[test]
    fn sanitized_link_uri_caps_length() {
        let long = format!("https://x.dev/{}", "a".repeat(MAX_URI_LEN));
        assert!(long.len() > MAX_URI_LEN);
        assert_eq!(sanitized_link_uri(&long), None);
        assert_eq!(sanitized_link_uri(""), None);
    }

    // --- Finding 2: forged-marker stripping at the trust boundary -----------

    #[test]
    fn strip_foreign_markers_removes_forged_iris_markers() {
        // A forged link OPEN+CLOSE pair, a forged file-ref, and the cursor
        // marker are all removed; the visible label survives verbatim.
        let forged = format!(
            "before{}label{}mid{}{}after",
            open_marker("https://evil.example/"),
            CLOSE_MARKER,
            fileref_open_marker("file:///etc/passwd"),
            "\x1b_iris:c\x07",
        );
        let cleaned = strip_foreign_markers(&forged);
        assert_eq!(cleaned, "beforelabelmidafter");
        assert!(!cleaned.contains('\x1b'));
        assert!(!cleaned.contains('\x07'));
        // A malformed forged marker (no terminator) is dropped to end-of-input
        // rather than left partially interpretable.
        assert_eq!(
            strip_foreign_markers("keep\x1b_iris:link:https://x"),
            "keep"
        );
        // Text with no Iris markers is borrowed untouched (no allocation).
        assert!(matches!(
            strip_foreign_markers("plain text"),
            std::borrow::Cow::Borrowed("plain text")
        ));
    }

    // --- Finding 4: diff-prefix file:line false positives -------------------

    #[test]
    fn find_file_refs_ignores_diff_prefixed_paths() {
        // A path glued to a unified-diff `-`/`+` marker is line content, not a
        // file reference.
        assert!(find_file_refs("-src/main.rs:12").is_empty());
        assert!(find_file_refs("+src/main.rs:12").is_empty());
        // Clean refs (line start or whitespace-separated) still linkify.
        assert_eq!(find_file_refs("src/main.rs:12").len(), 1);
        assert_eq!(find_file_refs(" at src/lib.rs:3").len(), 1);
        // Whitespace-separated after a diff marker still linkifies (the path
        // itself is clean).
        assert_eq!(find_file_refs("- src/main.rs:12").len(), 1);
        // An interior hyphen in a filename is still fine.
        assert_eq!(find_file_refs("my-file.rs:9").len(), 1);
    }
}
