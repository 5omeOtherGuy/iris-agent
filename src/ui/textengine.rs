//! Unified text engine: the single source of truth for the Iris TUI's display
//! width, ANSI/OSC/APC parsing, and width-aware wrap / truncate / slice.
//!
//! Modeled conceptually on pi-mono `packages/tui/src/utils.ts` (consulted, not
//! ported). It collapses three previously divergent implementations:
//!   * `tui/wrap.rs` width/wrap/truncate (was char-based)
//!   * `tui/text.rs` ANSI stripper (`strip_ansi_for_text` + CSI/OSC/APC consume)
//!   * `ui/text.rs` ANSI stripper + `visible_width` (the latter counted
//!     `chars().count()`, miscounting CJK/emoji -- fixed here).
//!
//! Width is measured by grapheme cluster so emoji ZWJ / VS16 / flag / combining
//! sequences are never split across a wrap or truncate boundary. `unicode-width`
//! 0.2 already returns the correct *cluster* width for a whole grapheme (ZWJ
//! sequences -> 2, combining mark + base -> base width); the engine's job is to
//! iterate by grapheme and keep clusters intact.
//!
//! ANSI handling: one parser recognizes CSI (`ESC [ ... final`), OSC / DCS / PM
//! / APC / SOS string controls (`ESC ] / P / ^ / _ / X ... BEL|ST`), and the
//! 8-bit C1 forms. The ANSI-aware wrap/truncate/slice carry SGR state across
//! physical rows and reopen OSC 8 hyperlinks on each row so a wrapped link stays
//! clickable -- a capability the ratatui span path cannot express (it drops OSC
//! 8 entirely). Emitting clickable links is a deferred UI feature; the engine
//! only *preserves* them.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------------------
// Width
// ---------------------------------------------------------------------------

/// Display width of `text` as the terminal renders it, summed over grapheme
/// clusters. ANSI escapes are NOT stripped (they are measured as their literal
/// width); callers that need the post-strip width use [`visible_width`]. This
/// matches the historical `wrap.rs::display_width` (raw `UnicodeWidthStr`),
/// which several call sites rely on as a conservative over-estimate (e.g. the
/// tool-output flood guard).
pub(crate) fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Display width of a single grapheme cluster.
pub(crate) fn cluster_width(cluster: &str) -> usize {
    UnicodeWidthStr::width(cluster)
}

/// Cluster width clamped to at least 1, so a zero-width / control cluster still
/// advances a wrap loop and never spins forever. Mirrors the old `char_width`.
pub(crate) fn cluster_advance(cluster: &str) -> usize {
    cluster_width(cluster).max(1)
}

/// Visible width after removing ANSI/OSC/APC escape sequences. This is the
/// correct measure for strings that may contain escapes; it replaces the buggy
/// `ui/text.rs::visible_width` which used `chars().count()`.
pub(crate) fn visible_width(text: &str) -> usize {
    display_width(&clean_text(text))
}

// ---------------------------------------------------------------------------
// ANSI / OSC / APC parsing
// ---------------------------------------------------------------------------

/// Remove ANSI/OSC/APC escape sequences but keep every other character
/// (including bare control characters such as TAB). General-purpose strip used
/// where the caller wants the raw text minus styling.
pub(crate) fn strip_ansi(input: &str) -> String {
    transform(input, false)
}

/// Remove ANSI/OSC/APC escape sequences AND remaining control characters,
/// producing a clean display string. Byte-for-byte compatible with the historic
/// `tui/text.rs::strip_ansi_for_text` (the `\r`-splitting / footer-cleaning
/// path).
pub(crate) fn clean_text(input: &str) -> String {
    transform(input, true)
}

fn transform(input: &str, drop_controls: bool) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\x1b' => match chars.peek() {
                Some('[') => {
                    chars.next();
                    consume_csi(&mut chars);
                }
                Some(']' | 'P' | '^' | '_' | 'X') => {
                    chars.next();
                    consume_string_control(&mut chars);
                }
                // ESC + any other single byte (e.g. ESC M): drop both.
                Some(_) => {
                    chars.next();
                }
                None => {}
            },
            // 8-bit C1 CSI introducer.
            '\u{009b}' => consume_csi(&mut chars),
            // 8-bit C1 string-control introducers: OSC, DCS, SOS, PM, APC.
            '\u{009d}' | '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => {
                consume_string_control(&mut chars);
            }
            _ if ch.is_control() => {
                if !drop_controls {
                    out.push(ch);
                }
            }
            _ => out.push(ch),
        }
    }
    out
}

fn consume_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    for ch in chars.by_ref() {
        // CSI final byte is in 0x40..=0x7e.
        if ('\u{40}'..='\u{7e}').contains(&ch) {
            break;
        }
    }
}

fn consume_string_control(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    while let Some(ch) = chars.next() {
        if ch == '\u{7}' {
            break;
        }
        if ch == '\x1b' && matches!(chars.peek(), Some('\\')) {
            chars.next();
            break;
        }
    }
}

// The single ANSI/OSC/APC escape scanner (`ansi_escape_len` + helpers) lives in
// the `ansi_aware` module below, next to its only callers.

// ---------------------------------------------------------------------------
// Plain (escape-free) truncate / wrap
// ---------------------------------------------------------------------------

/// Truncate `text` to at most `max` characters (grapheme-cluster boundary safe),
/// no ellipsis. Mirrors the historic `wrap.rs::truncate_chars` but never splits
/// a cluster.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    let mut out = String::new();
    for (count, cluster) in text.graphemes(true).enumerate() {
        if count >= max {
            break;
        }
        out.push_str(cluster);
    }
    out
}

/// Truncate `text` to at most `max` terminal columns (display width), stopping on
/// a grapheme-cluster boundary so wide/emoji/combining clusters are kept whole.
pub(crate) fn truncate_to_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0usize;
    for cluster in text.graphemes(true) {
        let w = cluster_width(cluster);
        if used + w > max {
            break;
        }
        out.push_str(cluster);
        used += w;
    }
    out
}

/// Greedy word-wrap `text` to `width` display columns, breaking at spaces. A
/// word that fits is moved whole onto its own row (so a URL/path stays
/// selectable as one unit); a word longer than the width hard-breaks at grapheme
/// boundaries. Returns at least one row (possibly empty). Grapheme-aware
/// successor to `wrap.rs::wrap_to_width`.
pub(crate) fn wrap_to_width(text: &str, width: usize) -> Vec<String> {
    if width == 0 || display_width(text) <= width {
        return vec![text.to_string()];
    }
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for (i, word) in text.split(' ').enumerate() {
        let word_w = display_width(word);
        if i > 0 && !cur.is_empty() && cur_w + 1 + word_w <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + word_w;
            continue;
        }
        if !cur.is_empty() {
            rows.push(std::mem::take(&mut cur));
            cur_w = 0;
        }
        if word_w <= width {
            cur.push_str(word);
            cur_w = word_w;
        } else {
            for cluster in word.graphemes(true) {
                let cw = cluster_advance(cluster);
                // Guard `!cur.is_empty()` so a single cluster wider than the
                // whole width never emits a phantom blank row before itself.
                if cur_w + cw > width && !cur.is_empty() {
                    rows.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                cur.push_str(cluster);
                cur_w += cw;
            }
        }
    }
    rows.push(cur);
    rows
}

// ---------------------------------------------------------------------------
// ANSI-aware wrap / truncate / slice (SGR + OSC 8 carry)
// ---------------------------------------------------------------------------
//
// This subsystem is the OSC 8 hyperlink-preserving, SGR-carrying string engine.
// It is exercised by the unit tests but not yet wired into a render path: the
// ratatui span pipeline already carries SGR via `Span.style`, and emitting
// clickable hyperlinks is a deferred UI feature. The engine only *preserves*
// OSC 8 so that future feature can use it. `allow(dead_code)` is scoped to this
// reserved subsystem rather than the whole module. Callers reach it as
// `textengine::ansi_aware::{wrap_ansi, truncate_ansi, slice_by_column}`.
pub(crate) mod ansi_aware {
    #![allow(dead_code)]
    use super::{cluster_advance, cluster_width, display_width, truncate_to_width, visible_width};
    use unicode_segmentation::UnicodeSegmentation;

    /// If an ANSI/OSC/APC escape sequence starts at byte `pos` in `s`, return its
    /// length in bytes; otherwise `None`. Mirrors pi-mono `extractAnsiCode`.
    fn ansi_escape_len(s: &str, pos: usize) -> Option<usize> {
        if pos >= s.len() {
            return None;
        }
        // ESC-introduced and 8-bit C1 introduced sequences. C1 bytes are encoded
        // as two UTF-8 bytes (0xC2 0x9b ..), so we match on the decoded char.
        let rest = &s[pos..];
        let mut iter = rest.char_indices();
        let (_, intro) = iter.next()?;
        match intro {
            '\x1b' => {
                let (_, second) = iter.next()?;
                match second {
                    '[' => csi_len(rest),
                    ']' | 'P' | '^' | '_' | 'X' => string_control_len(rest),
                    // ESC + single byte: 1 (ESC) + len(second).
                    _ => Some(rest.char_indices().nth(2).map_or(rest.len(), |(i, _)| i)),
                }
            }
            '\u{009b}' => csi_len(rest),
            '\u{009d}' | '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}' => {
                string_control_len(rest)
            }
            _ => None,
        }
    }

    fn csi_len(rest: &str) -> Option<usize> {
        // rest starts at the introducer: `ESC [` (two chars) or the 8-bit C1 CSI
        // (one char). Scan past the introducer, then return at the final byte
        // (0x40..=0x7e). `[` is itself in that range, so it must be skipped.
        // An unterminated sequence consumes to end-of-string: this keeps the
        // tokenizer linear (a dangling introducer is one token, not an O(n)
        // rescan from every following byte) and a dangling control has no width.
        let mut chars = rest.char_indices();
        let (_, first) = chars.next()?;
        if first == '\x1b' {
            chars.next(); // consume the '[' that completes the ESC-form introducer
        }
        for (i, ch) in chars {
            if ('\u{40}'..='\u{7e}').contains(&ch) {
                return Some(i + ch.len_utf8());
            }
        }
        Some(rest.len())
    }

    fn string_control_len(rest: &str) -> Option<usize> {
        // As with `csi_len`, an unterminated string control consumes to EOF so
        // the tokenizer stays linear on malformed input.
        let mut chars = rest.char_indices();
        // Skip introducer.
        chars.next();
        while let Some((i, ch)) = chars.next() {
            if ch == '\u{7}' {
                return Some(i + ch.len_utf8());
            }
            if ch == '\x1b'
                && let Some((j, '\\')) = chars.next()
            {
                return Some(j + '\\'.len_utf8());
            }
        }
        Some(rest.len())
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Osc8Term {
        Bel,
        St,
    }

    #[derive(Clone)]
    struct Hyperlink {
        params: String,
        url: String,
        term: Osc8Term,
    }

    impl Hyperlink {
        fn open(&self) -> String {
            let term = match self.term {
                Osc8Term::Bel => "\x07",
                Osc8Term::St => "\x1b\\",
            };
            format!("\x1b]8;{};{}{}", self.params, self.url, term)
        }
    }

    fn parse_osc8(code: &str) -> Option<Option<Hyperlink>> {
        // Returns:
        //   None        -> not an OSC 8 sequence
        //   Some(None)  -> OSC 8 close (empty url)
        //   Some(Some)  -> OSC 8 open
        // Accept both the 7-bit `ESC ] 8 ;` and the 8-bit C1 OSC `\u{009d}8;`
        // introducer, matching what the tokenizer recognizes.
        let intro_len = if code.starts_with("\x1b]8;") {
            "\x1b]8;".len()
        } else if code.starts_with("\u{009d}8;") {
            "\u{009d}8;".len()
        } else {
            return None;
        };
        let (term, body) = if let Some(b) = code.strip_suffix('\x07') {
            (Osc8Term::Bel, &b[intro_len..])
        } else if let Some(b) = code.strip_suffix("\x1b\\") {
            (Osc8Term::St, &b[intro_len..])
        } else {
            return None;
        };
        let sep = body.find(';')?;
        let params = body[..sep].to_string();
        let url = body[sep + 1..].to_string();
        if url.is_empty() {
            return Some(None);
        }
        Some(Some(Hyperlink { params, url, term }))
    }

    /// Tracks active SGR attributes and the active OSC 8 hyperlink so styling and
    /// links can be reopened on each wrapped physical row. Mirrors pi-mono's
    /// `AnsiCodeTracker`.
    #[derive(Default)]
    struct AnsiState {
        bold: bool,
        dim: bool,
        italic: bool,
        underline: bool,
        blink: bool,
        inverse: bool,
        hidden: bool,
        strike: bool,
        fg: Option<String>,
        bg: Option<String>,
        link: Option<Hyperlink>,
    }

    impl AnsiState {
        fn process(&mut self, code: &str) {
            if let Some(link) = parse_osc8(code) {
                self.link = link;
                return;
            }
            // Only SGR (`ESC [ ... m`, or the 8-bit C1 CSI `\u{009b} ... m`)
            // mutates style state.
            let body = code
                .strip_prefix('\x1b')
                .and_then(|c| c.strip_prefix('['))
                .or_else(|| code.strip_prefix('\u{009b}'));
            let Some(params) = body.and_then(|c| c.strip_suffix('m')) else {
                return;
            };
            if params.is_empty() || params == "0" {
                self.reset_sgr();
                return;
            }
            let parts: Vec<&str> = params.split(';').collect();
            let mut i = 0;
            while i < parts.len() {
                let Ok(n) = parts[i].parse::<u32>() else {
                    i += 1;
                    continue;
                };
                if n == 38 || n == 48 {
                    if parts.get(i + 1) == Some(&"5") {
                        if let Some(c) = parts.get(i + 2) {
                            let code = format!("{};5;{}", n, c);
                            if n == 38 {
                                self.fg = Some(code);
                            } else {
                                self.bg = Some(code);
                            }
                            i += 3;
                            continue;
                        }
                    } else if parts.get(i + 1) == Some(&"2")
                        && let (Some(r), Some(g), Some(b)) =
                            (parts.get(i + 2), parts.get(i + 3), parts.get(i + 4))
                    {
                        let code = format!("{};2;{};{};{}", n, r, g, b);
                        if n == 38 {
                            self.fg = Some(code);
                        } else {
                            self.bg = Some(code);
                        }
                        i += 5;
                        continue;
                    }
                }
                match n {
                    0 => self.reset_sgr(),
                    1 => self.bold = true,
                    2 => self.dim = true,
                    3 => self.italic = true,
                    4 => self.underline = true,
                    5 => self.blink = true,
                    7 => self.inverse = true,
                    8 => self.hidden = true,
                    9 => self.strike = true,
                    21 | 22 => {
                        self.bold = false;
                        if n == 22 {
                            self.dim = false;
                        }
                    }
                    23 => self.italic = false,
                    24 => self.underline = false,
                    25 => self.blink = false,
                    27 => self.inverse = false,
                    28 => self.hidden = false,
                    29 => self.strike = false,
                    39 => self.fg = None,
                    49 => self.bg = None,
                    30..=37 | 90..=97 => self.fg = Some(n.to_string()),
                    40..=47 | 100..=107 => self.bg = Some(n.to_string()),
                    _ => {}
                }
                i += 1;
            }
        }

        fn reset_sgr(&mut self) {
            self.bold = false;
            self.dim = false;
            self.italic = false;
            self.underline = false;
            self.blink = false;
            self.inverse = false;
            self.hidden = false;
            self.strike = false;
            self.fg = None;
            self.bg = None;
            // SGR reset does not clear an OSC 8 hyperlink.
        }

        fn has_sgr(&self) -> bool {
            self.bold
                || self.dim
                || self.italic
                || self.underline
                || self.blink
                || self.inverse
                || self.hidden
                || self.strike
                || self.fg.is_some()
                || self.bg.is_some()
        }

        /// SGR + hyperlink string needed to reopen the active state on a new row.
        fn active_codes(&self) -> String {
            let mut codes: Vec<String> = Vec::new();
            if self.bold {
                codes.push("1".into());
            }
            if self.dim {
                codes.push("2".into());
            }
            if self.italic {
                codes.push("3".into());
            }
            if self.underline {
                codes.push("4".into());
            }
            if self.blink {
                codes.push("5".into());
            }
            if self.inverse {
                codes.push("7".into());
            }
            if self.hidden {
                codes.push("8".into());
            }
            if self.strike {
                codes.push("9".into());
            }
            if let Some(fg) = &self.fg {
                codes.push(fg.clone());
            }
            if let Some(bg) = &self.bg {
                codes.push(bg.clone());
            }
            let mut out = if codes.is_empty() {
                String::new()
            } else {
                format!("\x1b[{}m", codes.join(";"))
            };
            if let Some(link) = &self.link {
                out.push_str(&link.open());
            }
            out
        }

        /// Closing reset to terminate a physical row without leaking state.
        fn close_codes(&self) -> String {
            let mut out = String::new();
            if self.link.is_some() {
                out.push_str("\x1b]8;;\x1b\\");
            }
            if self.has_sgr() {
                out.push_str("\x1b[0m");
            }
            out
        }
    }

    /// One token of an ANSI string: an escape sequence or a run of visible text.
    enum Token<'a> {
        Ansi(&'a str),
        Text(&'a str),
    }

    fn tokenize(line: &str) -> Vec<Token<'_>> {
        let mut toks = Vec::new();
        let mut i = 0;
        let mut text_start = 0;
        while i < line.len() {
            let is_escape_intro = line.as_bytes()[i] == 0x1b
                || line[i..].starts_with('\u{009b}')
                || is_c1_string(&line[i..]);
            if is_escape_intro && let Some(len) = ansi_escape_len(line, i) {
                if text_start < i {
                    toks.push(Token::Text(&line[text_start..i]));
                }
                toks.push(Token::Ansi(&line[i..i + len]));
                i += len;
                text_start = i;
                continue;
            }
            // advance one char
            let ch_len = line[i..].chars().next().map_or(1, |c| c.len_utf8());
            i += ch_len;
        }
        if text_start < line.len() {
            toks.push(Token::Text(&line[text_start..]));
        }
        toks
    }

    fn is_c1_string(s: &str) -> bool {
        matches!(
            s.chars().next(),
            Some('\u{009d}' | '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}')
        )
    }

    /// Wrap a string that may contain ANSI/OSC 8 sequences and `\n`, carrying SGR
    /// state and reopening OSC 8 hyperlinks across every physical row so a wrapped
    /// link stays clickable. Returns rows without trailing padding.
    pub(crate) fn wrap_ansi(text: &str, width: usize) -> Vec<String> {
        if text.is_empty() {
            return vec![String::new()];
        }
        let width = width.max(1);
        let mut result: Vec<String> = Vec::new();
        let mut carry = AnsiState::default();
        for input_line in text.split('\n') {
            let prefix = if result.is_empty() {
                String::new()
            } else {
                carry.active_codes()
            };
            let combined = format!("{prefix}{input_line}");
            for row in wrap_single_ansi(&combined, width) {
                result.push(row);
            }
            // Advance carry over this logical line's escapes for the next one.
            for tok in tokenize(input_line) {
                if let Token::Ansi(code) = tok {
                    carry.process(code);
                }
            }
        }
        if result.is_empty() {
            vec![String::new()]
        } else {
            result
        }
    }

    fn wrap_single_ansi(line: &str, width: usize) -> Vec<String> {
        if visible_width(line) <= width {
            return vec![line.to_string()];
        }
        let mut state = AnsiState::default();
        let mut rows: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut cur_w = 0usize;

        let flush = |rows: &mut Vec<String>, cur: &mut String, state: &AnsiState| {
            let mut row = std::mem::take(cur);
            row.push_str(&state.close_codes());
            rows.push(row);
        };

        for tok in tokenize(line) {
            match tok {
                Token::Ansi(code) => {
                    cur.push_str(code);
                    state.process(code);
                }
                Token::Text(run) => {
                    for cluster in run.graphemes(true) {
                        let cw = cluster_advance(cluster);
                        if cur_w + cw > width && cur_w > 0 {
                            flush(&mut rows, &mut cur, &state);
                            cur = state.active_codes();
                            cur_w = 0;
                        }
                        cur.push_str(cluster);
                        cur_w += cw;
                    }
                }
            }
        }
        flush(&mut rows, &mut cur, &state);
        rows
    }

    /// Truncate a string that may contain ANSI escapes to at most `max_width`
    /// visible columns, appending `ellipsis` when content is dropped, optionally
    /// padding with spaces to exactly `max_width`. Escapes are preserved and closed
    /// at the end. Mirrors pi-mono `truncateToWidth`.
    pub(crate) fn truncate_ansi(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
        if max_width == 0 {
            return String::new();
        }
        let total = visible_width(text);
        if total <= max_width {
            if pad {
                let mut out = text.to_string();
                out.push_str(&" ".repeat(max_width - total));
                return out;
            }
            return text.to_string();
        }
        let ellipsis_w = display_width(ellipsis);
        if ellipsis_w >= max_width {
            // The ellipsis alone does not fit: clip it to the field so the
            // result never exceeds max_width (matches pi-mono's behavior).
            let clipped = truncate_to_width(ellipsis, max_width);
            if pad {
                let w = display_width(&clipped);
                return format!("{clipped}{}", " ".repeat(max_width - w));
            }
            return clipped;
        }
        let target = max_width.saturating_sub(ellipsis_w);
        let mut state = AnsiState::default();
        let mut out = String::new();
        let mut used = 0usize;
        'outer: for tok in tokenize(text) {
            match tok {
                Token::Ansi(code) => {
                    out.push_str(code);
                    state.process(code);
                }
                Token::Text(run) => {
                    for cluster in run.graphemes(true) {
                        let w = cluster_width(cluster);
                        if used + w > target {
                            break 'outer;
                        }
                        out.push_str(cluster);
                        used += w;
                    }
                }
            }
        }
        out.push_str(ellipsis);
        used += ellipsis_w;
        out.push_str(&state.close_codes());
        if pad && used < max_width {
            out.push_str(&" ".repeat(max_width - used));
        }
        out
    }

    /// Extract the visible columns `[start, start + len)` from `line`, preserving
    /// ANSI escapes (escapes before the range are applied at the range start).
    /// Mirrors pi-mono `sliceByColumn`.
    pub(crate) fn slice_by_column(line: &str, start: usize, len: usize) -> String {
        if len == 0 {
            return String::new();
        }
        let end = start + len;
        let mut out = String::new();
        let mut pending = String::new();
        // Track active style across the slice so it is closed at the end and
        // never leaks into whatever the caller renders next.
        let mut state = AnsiState::default();
        let mut col = 0usize;
        let finish = |out: &mut String, state: &AnsiState| {
            if !out.is_empty() {
                out.push_str(&state.close_codes());
            }
        };
        for tok in tokenize(line) {
            match tok {
                Token::Ansi(code) => {
                    // Codes before the range set the style at the range start;
                    // either way they advance the tracker so the closing reset
                    // is correct.
                    state.process(code);
                    if col >= start && col < end {
                        out.push_str(code);
                    } else if col < start {
                        pending.push_str(code);
                    }
                }
                Token::Text(run) => {
                    for cluster in run.graphemes(true) {
                        let w = cluster_width(cluster);
                        if col >= start && col < end {
                            if !pending.is_empty() {
                                out.push_str(&pending);
                                pending.clear();
                            }
                            out.push_str(cluster);
                        }
                        col += w;
                        if col >= end {
                            finish(&mut out, &state);
                            return out;
                        }
                    }
                }
            }
        }
        finish(&mut out, &state);
        out
    }
}

/// Clip `content` to at most `remaining` display columns at a grapheme-cluster
/// boundary, returning the clipped text and its width. Used by the terminal
/// surface; grapheme-aware so an emoji/combining cluster at the right edge is
/// dropped whole rather than split into a broken half-cluster.
pub(crate) fn clip_to_width(content: &str, remaining: usize) -> (String, usize) {
    let full = display_width(content);
    if full <= remaining {
        return (content.to_string(), full);
    }
    let mut out = String::new();
    let mut width = 0usize;
    for cluster in content.graphemes(true) {
        let w = cluster_width(cluster);
        if width + w > remaining {
            break;
        }
        out.push_str(cluster);
        width += w;
    }
    (out, width)
}

#[cfg(test)]
mod tests {
    use super::ansi_aware::{slice_by_column, truncate_ansi, wrap_ansi};
    use super::*;

    #[test]
    fn display_width_matches_grapheme_sum_for_wide_and_emoji() {
        assert_eq!(display_width("hello"), 5);
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("e\u{301}"), 1); // combining acute
        assert_eq!(display_width("😀"), 2);
        assert_eq!(display_width("👨\u{200d}👩\u{200d}👧"), 2); // ZWJ family
        assert_eq!(display_width("🇺🇸"), 2); // flag
    }

    #[test]
    fn visible_width_strips_ansi_and_counts_display_columns() {
        // The historic bug: chars().count() returned 4 for "中文" inside SGR.
        assert_eq!(visible_width("\x1b[31m中文\x1b[0m"), 4);
        assert_eq!(visible_width("\x1b[1mhi\x1b[0m"), 2);
        // OSC 8 hyperlink wrapper contributes no width.
        assert_eq!(
            visible_width("\x1b]8;;https://x\x1b\\link\x1b]8;;\x1b\\"),
            4
        );
    }

    #[test]
    fn strip_ansi_keeps_controls_clean_text_drops_them() {
        assert_eq!(strip_ansi("\x1b[31ma\tb\x1b[0m"), "a\tb");
        assert_eq!(clean_text("\x1b[31ma\tb\x1b[0m"), "ab");
        // 8-bit C1 CSI and bracketed-paste markers are removed.
        assert_eq!(strip_ansi("\u{009b}31mx"), "x");
        assert_eq!(strip_ansi("\x1b[200~hi\x1b[201~"), "hi");
    }

    #[test]
    fn strip_ansi_handles_osc_with_st_and_bel() {
        assert_eq!(strip_ansi("\x1b]8;;https://a\x07txt\x1b]8;;\x07"), "txt");
        assert_eq!(strip_ansi("\x1b]0;title\x1b\\body"), "body");
    }

    #[test]
    fn truncate_to_width_keeps_clusters_whole() {
        assert_eq!(truncate_to_width("hello", 3), "hel");
        assert_eq!(truncate_to_width("中文字", 3), "中"); // 2 cols fit, next is 2
        // A combining mark stays attached to its base.
        assert_eq!(truncate_to_width("e\u{301}x", 1), "e\u{301}");
        // ZWJ emoji is kept whole or dropped, never split.
        assert_eq!(truncate_to_width("😀x", 2), "😀");
        assert_eq!(truncate_to_width("😀x", 1), "");
    }

    #[test]
    fn truncate_chars_counts_clusters() {
        assert_eq!(truncate_chars("abcdef", 3), "abc");
        assert_eq!(truncate_chars("a😀b", 2), "a😀");
    }

    #[test]
    fn wrap_to_width_breaks_at_spaces_and_hard_breaks_long_words() {
        assert_eq!(
            wrap_to_width("alpha beta gamma", 11),
            vec!["alpha beta", "gamma"]
        );
        assert_eq!(wrap_to_width("abcdefgh", 3), vec!["abc", "def", "gh"]);
        assert_eq!(wrap_to_width("short", 80), vec!["short"]);
    }

    #[test]
    fn wrap_to_width_keeps_emoji_clusters_intact() {
        // Two double-width emoji in a 2-col field must land on separate rows,
        // never split mid-cluster.
        let rows = wrap_to_width("😀😀", 2);
        assert_eq!(rows, vec!["😀", "😀"]);
    }

    #[test]
    fn wrap_ansi_carries_sgr_across_rows() {
        let rows = wrap_ansi("\x1b[31mred wrap here\x1b[0m", 4);
        assert!(rows.len() > 1);
        // Every row after the first reopens the red SGR.
        for row in &rows[1..] {
            assert!(
                row.starts_with("\x1b[31m"),
                "row did not reopen SGR: {row:?}"
            );
        }
        // Visible text reconstructs the original.
        let visible: String = rows
            .iter()
            .map(|r| strip_ansi(r))
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(visible.replace(' ', ""), "redwraphere");
    }

    #[test]
    fn wrap_ansi_reopens_osc8_hyperlink_on_each_row() {
        let link = "\x1b]8;;https://example.com/very/long\x1b\\example link text\x1b]8;;\x1b\\";
        let rows = wrap_ansi(link, 6);
        assert!(rows.len() > 1);
        for row in &rows {
            assert!(
                row.contains("\x1b]8;;https://example.com/very/long"),
                "row missing reopened hyperlink: {row:?}"
            );
        }
    }

    #[test]
    fn truncate_ansi_preserves_style_and_appends_ellipsis() {
        let out = truncate_ansi("\x1b[31mhello world\x1b[0m", 8, "…", false);
        assert_eq!(visible_width(&out), 8); // 7 chars + ellipsis
        assert!(out.starts_with("\x1b[31m"));
        assert!(out.contains('…'));
        assert!(out.trim_end().ends_with("\x1b[0m"));
    }

    #[test]
    fn truncate_ansi_pads_to_exact_width() {
        let out = truncate_ansi("hi", 5, "…", true);
        assert_eq!(out, "hi   ");
        assert_eq!(visible_width(&out), 5);
    }

    #[test]
    fn slice_by_column_extracts_visible_range_with_style() {
        // columns 2..5 of "abcdef" -> "cde"
        assert_eq!(slice_by_column("abcdef", 2, 3), "cde");
        // styling before the range is applied at the range start
        let out = slice_by_column("\x1b[31mabcdef\x1b[0m", 2, 3);
        assert!(out.starts_with("\x1b[31m"));
        assert_eq!(strip_ansi(&out), "cde");
    }

    #[test]
    fn wrap_ansi_carries_rgb_and_256_color_across_rows() {
        let rgb = wrap_ansi("\x1b[38;2;10;20;30mrgb wrap line\x1b[0m", 4);
        assert!(rgb.len() > 1);
        for row in &rgb[1..] {
            assert!(
                row.starts_with("\x1b[38;2;10;20;30m"),
                "RGB fg not carried: {row:?}"
            );
        }
        let c256 = wrap_ansi("\x1b[38;5;240mxterm wrap\x1b[0m", 4);
        for row in &c256[1..] {
            assert!(
                row.starts_with("\x1b[38;5;240m"),
                "256 fg not carried: {row:?}"
            );
        }
    }

    #[test]
    fn truncate_ansi_preserves_osc8_hyperlink() {
        let link = "\x1b]8;;https://example.com\x1b\\link text here\x1b]8;;\x1b\\";
        let out = truncate_ansi(link, 6, "\u{2026}", false);
        assert!(
            out.contains("\x1b]8;;https://example.com"),
            "link open lost: {out:?}"
        );
        assert!(out.contains("\x1b]8;;\x1b\\"), "link not closed: {out:?}");
        assert!(out.contains('\u{2026}'));
    }

    #[test]
    fn truncate_ansi_never_exceeds_width_when_ellipsis_too_wide() {
        // ellipsis "..." (3 cols) wider than max_width 1 must still fit.
        assert!(visible_width(&truncate_ansi("hello", 1, "...", false)) <= 1);
        assert_eq!(truncate_ansi("hello", 2, "...", false), "..");
    }

    #[test]
    fn slice_by_column_closes_style_and_does_not_leak() {
        // A slice that ends while red is active must emit a closing reset.
        let out = slice_by_column("\x1b[31mredtext\x1b[0m", 0, 3);
        assert_eq!(strip_ansi(&out), "red");
        assert!(out.starts_with("\x1b[31m"));
        assert!(out.trim_end().ends_with("\x1b[0m"), "style leaked: {out:?}");
    }

    #[test]
    fn tokenize_is_linear_on_unterminated_escapes() {
        // Many unterminated OSC introducers must not blow up (was O(n^2));
        // this completes quickly and treats the dangling run as zero width.
        let pathological = "\x1b]".repeat(20_000);
        // Dangling OSC introducers have no visible width.
        assert_eq!(visible_width(&pathological), 0);
        // The ANSI-aware wrap of the same input also terminates quickly.
        let rows = wrap_ansi(&pathological, 10);
        assert!(!rows.is_empty());
    }

    #[test]
    fn clip_to_width_matches_unicode_width_for_plain_and_keeps_clusters() {
        assert_eq!(clip_to_width("abcdef", 5), ("abcde".to_string(), 5));
        assert_eq!(clip_to_width("ab", 5), ("ab".to_string(), 2));
        assert_eq!(clip_to_width("中文", 3), ("中".to_string(), 2));
        // ZWJ cluster dropped whole, not split.
        assert_eq!(clip_to_width("😀x", 1), (String::new(), 0));
    }
}
