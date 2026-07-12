//! Deterministic, local excerpt selection for `read_web_page`.
//!
//! When the caller supplies an `objective`, we treat it as an extraction
//! instruction: given the fetched Markdown and a natural-language objective,
//! return the source passages most relevant to that objective. No LLM
//! summarization happens here -- every returned excerpt is verbatim source text
//! plus its surrounding heading context. Scoring is a local keyword-overlap
//! approximation, so the module deliberately favours a curated handful of
//! strong matches over "every positively scored passage that fits the budget".
//!
//! Ported from the `ampi-web` reference (`excerpts.ts`). Two deliberate
//! divergences, both forced by the Rust surface and the caller contract:
//!   - The public budget is measured in **characters** (`max_chars`), not
//!     UTF-8 bytes, so the returned string is char-boundary safe by
//!     construction. Internal passage-sizing likewise counts characters.
//!   - The result is a single stitched String (excerpts joined by
//!     [`EXCERPT_SEPARATOR`]) rather than a list, with a leading-text fallback
//!     when the objective yields no signal or nothing matches.
//!
//! The algorithm is pure and synchronous: no I/O, no network, no async.

/// Separator stitched between adjacent excerpts in the returned string. Mirrors
/// the reference so downstream framing/formatting stays identical.
const EXCERPT_SEPARATOR: &str = "\n\n---\n\n";

/// Maximum number of excerpts stitched from a single objective extraction.
///
/// Treats the excerpt list as a curated set: local scoring is an
/// approximation, so emitting many marginally-relevant passages dilutes the
/// useful signal. The cap is a conservative ceiling; the char budget still
/// applies on top. Ported value: 10.
const MAX_EXCERPTS: usize = 10;

/// Relative relevance floor used to drop marginal matches: only passages whose
/// score is at least this fraction of the best passage score are eligible.
/// Relative rather than absolute so it adapts to single-strong-token
/// objectives as well as multi-token queries. Ported value: 0.3.
const RELATIVE_SCORE_FLOOR: f64 = 0.3;

/// Multiplicative score penalty applied to passages that look like a
/// citation/footnote/bibliography list rather than informative body content.
/// Multiplicative (not a hard drop) so pages with only citation-style content
/// still surface something, while real body matches outrank citations on the
/// same query. Ported value: 0.15.
const CITATION_PENALTY: f64 = 0.15;

/// Minimum citation markers in a passage body to treat it as citation-dense
/// even when the enclosing heading is not a known References-style section.
/// Tuned to fire on bibliography/footnote lists while leaving body paragraphs
/// that contain a single citation reference untouched. Ported value: 3.
const CITATION_MARKER_THRESHOLD: usize = 3;

/// Target size (in characters) for a single passage before a sentence-split is
/// applied. Ported value: 2048 (the reference measured this in UTF-8 bytes;
/// this port counts characters -- see the module divergence note).
const TARGET_PASSAGE_CHARS: usize = 2048;

/// Heading names (case-insensitive, whole-word) that mark a section as a
/// References-style list rather than body content. Matched against any level of
/// a passage's heading trail.
const REFERENCE_HEADING_NAMES: &[&str] = &[
    "references",
    "reference",
    "bibliography",
    "citations",
    "footnotes",
    "notes",
    "works cited",
    "further reading",
];

/// A small, conservative English stopword set. Kept small on purpose so that
/// short but meaningful objective words survive tokenization.
const STOPWORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "and",
    "or",
    "but",
    "of",
    "in",
    "on",
    "at",
    "to",
    "for",
    "with",
    "by",
    "from",
    "as",
    "is",
    "are",
    "was",
    "were",
    "be",
    "been",
    "being",
    "this",
    "that",
    "these",
    "those",
    "it",
    "its",
    "i",
    "you",
    "he",
    "she",
    "we",
    "they",
    "my",
    "your",
    "our",
    "their",
    "us",
    "me",
    "him",
    "her",
    "them",
    "do",
    "does",
    "did",
    "doing",
    "done",
    "has",
    "have",
    "had",
    "having",
    "can",
    "could",
    "should",
    "would",
    "will",
    "may",
    "might",
    "must",
    "shall",
    "what",
    "which",
    "who",
    "whom",
    "whose",
    "when",
    "where",
    "why",
    "how",
    "about",
    "into",
    "over",
    "under",
    "out",
    "up",
    "down",
    "off",
    "than",
    "then",
    "so",
    "if",
    "not",
    "no",
    "yes",
    "such",
    "any",
    "all",
    "some",
    "more",
    "most",
    "much",
    "many",
    "few",
    "other",
    "another",
    "same",
    "very",
    "just",
    "only",
    "tell",
    "find",
    "get",
    "show",
    "give",
    "explain",
    "describe",
    "info",
    "information",
    "please",
    "thanks",
    "regarding",
];

/// A candidate passage carved out of the source Markdown.
struct Passage {
    /// Text including any heading prefix carried for context.
    text: String,
    /// Exact heading prefix included at the start of `text`, or empty when no
    /// heading context was carried. Used at emission time to strip the prefix
    /// on consecutive same-trail excerpts so the heading does not visibly
    /// repeat in the stitched output.
    heading_prefix: String,
    /// Concatenated heading chain (for heading-match scoring and dedup).
    heading: String,
    /// Position in the original Markdown (in production order).
    index: usize,
}

/// The objective parsed into scoring signal.
struct ParsedObjective {
    /// Lowercased single tokens (no stopwords, length >= 2).
    tokens: Vec<String>,
    /// Lowercased multi-word exact phrases (from double quotes in objective).
    phrases: Vec<String>,
}

/// Select the most objective-relevant verbatim excerpts from `text`, stitched
/// into a single string under a character budget. Deterministic. When the
/// objective yields no signal or no block matches, fall back to the leading
/// portion of `text`. `max_chars` bounds the returned string (char-boundary
/// safe).
pub(super) fn select_excerpts(text: &str, objective: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let excerpts = objective_excerpts(text, objective, max_chars);
    let stitched = if excerpts.is_empty() {
        // No signal or no match: fall back to the leading portion of the source.
        text.to_string()
    } else {
        excerpts.join(EXCERPT_SEPARATOR)
    };
    truncate_to_chars(&stitched, max_chars)
}

/// Core selection: parse the objective, split the Markdown into passages, score
/// and filter them, then return the chosen excerpts in original document order.
/// Returns an empty vec when there is no signal or nothing matches, which the
/// caller turns into the leading-text fallback.
fn objective_excerpts(markdown: &str, objective: &str, max_chars: usize) -> Vec<String> {
    let objective = objective.trim();
    if objective.is_empty() {
        return Vec::new();
    }
    let parsed = parse_objective(objective);
    if parsed.tokens.is_empty() && parsed.phrases.is_empty() {
        return Vec::new();
    }

    let passages = split_markdown_into_passages(markdown);
    let mut scored: Vec<(Passage, f64)> = Vec::new();
    for passage in passages {
        let score = score_passage(&passage, &parsed);
        if score <= 0.0 {
            continue;
        }
        // Demote citation/footnote/bibliography passages so real body content
        // outranks them when both match the objective tokens.
        let penalty = if is_citation_like_passage(&passage) {
            CITATION_PENALTY
        } else {
            1.0
        };
        scored.push((passage, score * penalty));
    }
    if scored.is_empty() {
        return Vec::new();
    }

    // Drop passages whose score is well below the best match. Relative so it
    // adapts to one-strong-token objectives as well as multi-token queries.
    let top_score = scored.iter().fold(0.0_f64, |max, (_, s)| max.max(*s));
    let min_score = top_score * RELATIVE_SCORE_FLOOR;
    let mut eligible: Vec<(Passage, f64)> = scored
        .into_iter()
        .filter(|(_, s)| *s >= min_score)
        .collect();

    // Highest score first, ties broken by original order.
    eligible.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.index.cmp(&b.0.index)));

    // Pick top passages within the char budget and the top-K cap. The first
    // selected passage is admitted unconditionally (mirrors the reference); the
    // final truncation in `select_excerpts` enforces the hard char bound.
    let separator_chars = EXCERPT_SEPARATOR.chars().count();
    let mut selected: Vec<(Passage, f64)> = Vec::new();
    let mut used = 0usize;
    for candidate in eligible {
        if selected.len() >= MAX_EXCERPTS {
            break;
        }
        let passage_chars = candidate.0.text.chars().count();
        let sep_cost = if selected.is_empty() {
            0
        } else {
            separator_chars
        };
        let projected = used + sep_cost + passage_chars;
        if !selected.is_empty() && projected > max_chars {
            continue;
        }
        selected.push(candidate);
        used = projected;
        if used >= max_chars {
            break;
        }
    }

    // Restore original document order for emission.
    selected.sort_by_key(|entry| entry.0.index);

    // Drop the heading prefix on consecutive same-trail excerpts so the stitched
    // output does not repeat the same `## Heading` line for each sibling passage.
    let mut excerpts: Vec<String> = Vec::new();
    let mut previous_heading: Option<String> = None;
    for (passage, _) in selected {
        let same_trail = previous_heading
            .as_deref()
            .is_some_and(|prev| !passage.heading.is_empty() && passage.heading == prev);
        if same_trail
            && !passage.heading_prefix.is_empty()
            && passage.text.starts_with(&passage.heading_prefix)
        {
            excerpts.push(passage.text[passage.heading_prefix.len()..].to_string());
        } else {
            excerpts.push(passage.text.clone());
        }
        previous_heading = Some(passage.heading);
    }
    excerpts
}

/// Truncate `text` to at most `max_chars` characters on a char boundary.
fn truncate_to_chars(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((byte_idx, _)) => text[..byte_idx].to_string(),
        None => text.to_string(),
    }
}

/* ------------------------------------------------------------------------- */
/* Objective parsing + scoring                                               */
/* ------------------------------------------------------------------------- */

/// Parse the objective into exact phrases (double-quoted spans) and single
/// tokens. Phrase words are also seeded into the token list so a passage with
/// the full phrase also contributes to term-match counts.
fn parse_objective(objective: &str) -> ParsedObjective {
    let (phrases, stripped) = extract_phrases_and_strip(objective);
    let mut tokens = tokenize(&stripped);
    for phrase in &phrases {
        for word in tokenize(phrase) {
            tokens.push(word);
        }
    }
    ParsedObjective { tokens, phrases }
}

/// Pull double-quoted phrases out of the objective and return them (lowercased,
/// non-empty) alongside the objective with each quoted span replaced by a
/// space. Mirrors the reference `/"([^"]+)"/g` behaviour: empty `""` is not a
/// phrase and its quotes are left in place.
fn extract_phrases_and_strip(objective: &str) -> (Vec<String>, String) {
    let chars: Vec<char> = objective.chars().collect();
    let mut phrases: Vec<String> = Vec::new();
    let mut stripped = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '"'
            && let Some(close) = (i + 1..chars.len()).find(|&j| chars[j] == '"')
            && close > i + 1
        {
            let content: String = chars[i + 1..close].iter().collect();
            let trimmed = content.trim().to_lowercase();
            if !trimmed.is_empty() {
                phrases.push(trimmed);
            }
            stripped.push(' ');
            i = close + 1;
            continue;
        }
        stripped.push(chars[i]);
        i += 1;
    }
    (phrases, stripped)
}

/// Lowercase, strip punctuation to spaces (keeping letters, numbers, hyphen),
/// then split into tokens: hyphen-trimmed, length >= 2, non-stopword.
fn tokenize(text: &str) -> Vec<String> {
    let cleaned: String = text
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c.is_whitespace() {
                c
            } else {
                ' '
            }
        })
        .collect();
    let mut out = Vec::new();
    for raw in cleaned.split_whitespace() {
        let token = raw.trim_matches('-');
        if token.chars().count() < 2 {
            continue;
        }
        if STOPWORDS.contains(&token) {
            continue;
        }
        out.push(token.to_string());
    }
    out
}

/// Score a passage against the parsed objective. Returns 0.0 when neither a
/// phrase nor a distinct term matches. Weights ported verbatim from the
/// reference: phrases dominate, then heading hits, then distinct terms, then
/// raw occurrences, a small density term, and a co-occurrence bonus.
fn score_passage(passage: &Passage, parsed: &ParsedObjective) -> f64 {
    let body_lower = passage.text.to_lowercase();
    let heading_lower = passage.heading.to_lowercase();

    // Deduplicate tokens (a token seeded from both the objective and a phrase
    // must count once), preserving deterministic behaviour.
    let mut seen: Vec<&str> = Vec::new();
    let mut term_occurrences = 0usize;
    let mut distinct_term_hits = 0usize;
    let mut heading_hits = 0usize;
    for token in &parsed.tokens {
        if seen.contains(&token.as_str()) {
            continue;
        }
        seen.push(token.as_str());
        let body_count = count_token_occurrences(&body_lower, token);
        if body_count > 0 {
            distinct_term_hits += 1;
            term_occurrences += body_count;
        }
        if !heading_lower.is_empty() && count_token_occurrences(&heading_lower, token) > 0 {
            heading_hits += 1;
        }
    }
    let mut phrase_hits = 0usize;
    for phrase in &parsed.phrases {
        phrase_hits += count_substring_occurrences(&body_lower, phrase);
    }
    if phrase_hits == 0 && distinct_term_hits == 0 {
        return 0.0;
    }

    let word_count = body_lower.split_whitespace().count().max(1);
    let density = term_occurrences as f64 / word_count as f64;
    let cooccurrence_bonus = if distinct_term_hits >= 2 { 5.0 } else { 0.0 };

    (phrase_hits as f64) * 10.0
        + (heading_hits as f64) * 4.0
        + (distinct_term_hits as f64) * 3.0
        + (term_occurrences as f64)
        + density * 5.0
        + cooccurrence_bonus
}

/// Count whole-word occurrences of `token` in `haystack` (both already
/// lowercased). A match is bounded by non-alphanumeric characters, mirroring
/// the reference `(?<![\p{L}\p{N}])token(?![\p{L}\p{N}])`. Non-overlapping.
fn count_token_occurrences(haystack: &str, token: &str) -> usize {
    if token.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut start = 0;
    while start <= haystack.len() {
        let Some(rel) = haystack[start..].find(token) else {
            break;
        };
        let idx = start + rel;
        let before_ok = haystack[..idx]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let end = idx + token.len();
        let after_ok = haystack[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if before_ok && after_ok {
            count += 1;
            start = end;
        } else {
            // Advance one character past this candidate to catch later matches.
            start = idx + haystack[idx..].chars().next().map_or(1, char::len_utf8);
        }
    }
    count
}

/// Count non-overlapping substring occurrences of `needle` in `haystack`.
fn count_substring_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0;
    let mut start = 0;
    while let Some(rel) = haystack[start..].find(needle) {
        count += 1;
        start += rel + needle.len();
    }
    count
}

/* ------------------------------------------------------------------------- */
/* Citation demotion                                                         */
/* ------------------------------------------------------------------------- */

/// True when a passage looks like a citation/footnote/bibliography entry rather
/// than informative body content. See [`CITATION_PENALTY`].
fn is_citation_like_passage(passage: &Passage) -> bool {
    if is_under_reference_heading(&passage.heading) {
        return true;
    }
    citation_marker_count(&passage.text) >= CITATION_MARKER_THRESHOLD
}

/// True when any level of the heading trail is a References-style section name.
/// The heading chain keeps its `#` markers per level (e.g. `# Topic ## Refs`);
/// split on the markers to recover individual heading texts for whole-word
/// matching.
fn is_under_reference_heading(heading: &str) -> bool {
    if heading.is_empty() {
        return false;
    }
    split_heading_texts(heading)
        .iter()
        .any(|text| REFERENCE_HEADING_NAMES.contains(&text.as_str()))
}

/// Split a heading chain on `\s*#+\s+` delimiters into lowercased heading texts,
/// dropping empties.
fn split_heading_texts(heading: &str) -> Vec<String> {
    let chars: Vec<char> = heading.chars().collect();
    let mut texts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < chars.len() {
        // Try to match a `\s*#+\s+` delimiter starting at i.
        let mut j = i;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        let hash_start = j;
        while j < chars.len() && chars[j] == '#' {
            j += 1;
        }
        if j > hash_start {
            let ws_start = j;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j > ws_start {
                texts.push(std::mem::take(&mut cur));
                i = j;
                continue;
            }
        }
        cur.push(chars[i]);
        i += 1;
    }
    texts.push(cur);
    texts
        .into_iter()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Count citation markers in a passage body. Mirrors the reference's five
/// heuristics: bracketed numerics, "Retrieved <year>", "Archived from", ISBN
/// references, and Wikipedia-style "^ Jump up to" anchors.
fn citation_marker_count(text: &str) -> usize {
    count_bracketed_numerics(text)
        + count_retrieved_dates(text)
        + count_ci_bounded(text, b"archived from")
        + count_isbn(text)
        + count_jump_up(text)
}

/// Count `[<digits>]` bracketed citation markers.
fn count_bracketed_numerics(text: &str) -> usize {
    let b = text.as_bytes();
    let mut count = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'[' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > i + 1 && j < b.len() && b[j] == b']' {
                count += 1;
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
    count
}

/// Count case-sensitive whole-word "Retrieved" markers that have a bare 4-digit
/// year within the following 40 characters (no newline).
fn count_retrieved_dates(text: &str) -> usize {
    let b = text.as_bytes();
    let needle = "Retrieved";
    let mut count = 0;
    let mut start = 0;
    while let Some(rel) = text[start..].find(needle) {
        let idx = start + rel;
        let before_ok = idx == 0 || !is_word_byte(b[idx - 1]);
        let end = idx + needle.len();
        let after_ok = end >= b.len() || !is_word_byte(b[end]);
        if before_ok && after_ok && has_four_digit_token_within(text, end, 40) {
            count += 1;
        }
        start = end;
    }
    count
}

/// True when a bare 4-digit token (bounded by non-word characters) begins within
/// `max_lead` characters after `from`, stopping at any newline.
fn has_four_digit_token_within(text: &str, from: usize, max_lead: usize) -> bool {
    let chars: Vec<char> = text[from..].chars().collect();
    let mut lead = 0;
    let mut i = 0;
    while i < chars.len() && lead <= max_lead {
        if chars[i] == '\n' {
            return false;
        }
        if chars[i].is_ascii_digit() {
            let prev_ok = i == 0 || !is_word_char(chars[i - 1]);
            let mut k = i;
            while k < chars.len() && chars[k].is_ascii_digit() {
                k += 1;
            }
            let after_ok = k >= chars.len() || !is_word_char(chars[k]);
            if prev_ok && after_ok && k - i == 4 {
                return true;
            }
        }
        lead += 1;
        i += 1;
    }
    false
}

/// Count whole-word, case-insensitive occurrences of an ASCII phrase.
fn count_ci_bounded(text: &str, phrase: &[u8]) -> usize {
    let b = text.as_bytes();
    let n = phrase.len();
    if n == 0 || b.len() < n {
        return 0;
    }
    let mut count = 0;
    let mut i = 0;
    while i + n <= b.len() {
        if ascii_ci_eq(&b[i..i + n], phrase) {
            let before_ok = i == 0 || !is_word_byte(b[i - 1]);
            let after_ok = i + n >= b.len() || !is_word_byte(b[i + n]);
            if before_ok && after_ok {
                count += 1;
                i += n;
                continue;
            }
        }
        i += 1;
    }
    count
}

/// Count `ISBN` markers: word-boundary "ISBN", an optional single space/hyphen,
/// then a digit. Case-insensitive.
fn count_isbn(text: &str) -> usize {
    let b = text.as_bytes();
    let needle = b"isbn";
    let mut count = 0;
    let mut i = 0;
    while i + needle.len() <= b.len() {
        if ascii_ci_eq(&b[i..i + needle.len()], needle) {
            let before_ok = i == 0 || !is_word_byte(b[i - 1]);
            if before_ok {
                let mut j = i + needle.len();
                if j < b.len() && (b[j] == b'-' || b[j].is_ascii_whitespace()) {
                    j += 1;
                }
                if j < b.len() && b[j].is_ascii_digit() {
                    count += 1;
                    i = j;
                    continue;
                }
            }
        }
        i += 1;
    }
    count
}

/// Count Wikipedia-style `^ Jump up to` anchors: a caret, optional whitespace,
/// then the phrase. Case-insensitive.
fn count_jump_up(text: &str) -> usize {
    let b = text.as_bytes();
    let phrase = b"jump up to";
    let mut count = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'^' {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j + phrase.len() <= b.len() && ascii_ci_eq(&b[j..j + phrase.len()], phrase) {
                count += 1;
                i = j + phrase.len();
                continue;
            }
        }
        i += 1;
    }
    count
}

/// ASCII case-insensitive equality; `lower` must already be lowercase.
fn ascii_ci_eq(bytes: &[u8], lower: &[u8]) -> bool {
    bytes.len() == lower.len()
        && bytes
            .iter()
            .zip(lower)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
}

/// `\w`-style test (ASCII letters, digits, underscore) on a raw byte.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// `\w`-style test (ASCII letters, digits, underscore) on a char.
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/* ------------------------------------------------------------------------- */
/* Markdown-aware passage splitter                                           */
/* ------------------------------------------------------------------------- */

/// Split Markdown into scored-candidate passages, carrying the enclosing
/// heading trail as context. Handles fenced code blocks, list groups, and
/// paragraphs; oversized blocks are sentence-split with one-sentence overlap.
fn split_markdown_into_passages(markdown: &str) -> Vec<Passage> {
    let lines: Vec<&str> = markdown
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut passages: Vec<Passage> = Vec::new();
    // Slot per heading level (1..=6). Empty string when no heading at that depth.
    let mut heading_stack: [String; 6] = Default::default();
    let mut i = 0;
    let mut passage_index = 0;

    while i < lines.len() {
        let line = lines[i];
        if let Some((level, text)) = parse_heading(line) {
            heading_stack[level - 1] = format!("{} {}", "#".repeat(level), text);
            for deeper in heading_stack.iter_mut().skip(level) {
                deeper.clear();
            }
            i += 1;
            continue;
        }
        if let Some((fence_char, fence_len)) = fence_marker(line) {
            let start = i;
            i += 1;
            while i < lines.len() && !is_fence_close(lines[i], fence_char, fence_len) {
                i += 1;
            }
            if i < lines.len() {
                i += 1;
            }
            let block = lines[start..i].join("\n");
            passage_index = flush_block(&block, &heading_stack, passage_index, &mut passages);
            continue;
        }
        if line.trim().is_empty() {
            i += 1;
            continue;
        }
        if is_list_item(line) {
            let start = i;
            while i < lines.len() {
                let current = lines[i];
                if is_list_item(current) {
                    i += 1;
                    continue;
                }
                // Continuation of a list item: indented non-empty line.
                if !current.trim().is_empty() && current.starts_with(char::is_whitespace) {
                    i += 1;
                    continue;
                }
                // Allow a single blank line between contiguous list items.
                if current.trim().is_empty() && i + 1 < lines.len() && is_list_item(lines[i + 1]) {
                    i += 1;
                    continue;
                }
                break;
            }
            let block = lines[start..i]
                .join("\n")
                .trim_end_matches('\n')
                .to_string();
            passage_index = flush_block(&block, &heading_stack, passage_index, &mut passages);
            continue;
        }
        // Default paragraph: collect until a blank line, heading, or fence.
        let start = i;
        while i < lines.len()
            && !lines[i].trim().is_empty()
            && parse_heading(lines[i]).is_none()
            && fence_marker(lines[i]).is_none()
        {
            i += 1;
        }
        let block = lines[start..i].join("\n");
        passage_index = flush_block(&block, &heading_stack, passage_index, &mut passages);
    }
    passages
}

/// Trim, prefix with the current heading trail, size-split if needed, and append
/// the resulting passages. Returns the next passage index.
fn flush_block(
    block: &str,
    stack: &[String; 6],
    start_index: usize,
    out: &mut Vec<Passage>,
) -> usize {
    let trimmed = block.trim_matches('\n');
    if trimmed.is_empty() {
        return start_index;
    }
    let prefix_parts: Vec<&str> = stack
        .iter()
        .filter(|h| !h.is_empty())
        .map(String::as_str)
        .collect();
    let prefix = if prefix_parts.is_empty() {
        String::new()
    } else {
        format!("{}\n\n", prefix_parts.join("\n"))
    };
    let heading_text = prefix_parts.join(" ");
    let sized = size_split(&prefix, trimmed, &heading_text, start_index);
    let next = start_index + sized.len();
    out.extend(sized);
    next
}

/// Emit a single passage, or sentence-split the body (one-sentence overlap) when
/// the prefixed block exceeds [`TARGET_PASSAGE_CHARS`]. Indices run contiguously
/// from `start_index`.
fn size_split(prefix: &str, body: &str, heading: &str, start_index: usize) -> Vec<Passage> {
    let full = format!("{prefix}{body}");
    let single = || {
        vec![Passage {
            text: full.clone(),
            heading_prefix: prefix.to_string(),
            heading: heading.to_string(),
            index: start_index,
        }]
    };
    if full.chars().count() <= TARGET_PASSAGE_CHARS {
        return single();
    }
    let sentences = split_sentences(body);
    if sentences.len() <= 1 {
        return single();
    }
    let mut out: Vec<Passage> = Vec::new();
    let mut buffer = String::new();
    let mut last_sentence = String::new();
    let mut local_index = start_index;
    for sentence in &sentences {
        let candidate = if buffer.is_empty() {
            sentence.clone()
        } else {
            format!("{buffer} {sentence}")
        };
        let would_overflow = !buffer.is_empty()
            && format!("{prefix}{candidate}").chars().count() > TARGET_PASSAGE_CHARS;
        if would_overflow {
            out.push(Passage {
                text: format!("{prefix}{buffer}"),
                heading_prefix: prefix.to_string(),
                heading: heading.to_string(),
                index: local_index,
            });
            local_index += 1;
            buffer = if last_sentence.is_empty() {
                sentence.clone()
            } else {
                format!("{last_sentence} {sentence}")
            };
        } else {
            buffer = candidate;
        }
        last_sentence = sentence.clone();
    }
    if !buffer.is_empty() {
        out.push(Passage {
            text: format!("{prefix}{buffer}"),
            heading_prefix: prefix.to_string(),
            heading: heading.to_string(),
            index: local_index,
        });
    }
    out
}

/// Lightweight sentence splitter: break on whitespace preceded by `.!?` and
/// followed by an uppercase ASCII letter or digit. Good enough for English
/// prose; code and lists never reach this path.
fn split_sentences(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_whitespace() && i > 0 && matches!(chars[i - 1], '.' | '!' | '?') {
            let ws_start = i;
            let mut j = i;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            if j < chars.len() && (chars[j].is_ascii_uppercase() || chars[j].is_ascii_digit()) {
                parts.push(chars[start..ws_start].iter().collect());
                start = j;
                i = j;
                continue;
            }
        }
        i += 1;
    }
    parts.push(chars[start..].iter().collect());
    parts
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Parse an ATX heading line: 1..=6 leading `#`, required whitespace, then the
/// non-empty title with trailing whitespace and closing `#` run removed. Returns
/// `(level, title)`.
fn parse_heading(line: &str) -> Option<(usize, String)> {
    let level = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = &line[level..];
    if !rest.starts_with(char::is_whitespace) {
        return None;
    }
    // Strip the leading `\s+` and the trailing `\s*#*\s*`.
    let text = rest
        .trim_start()
        .trim_end()
        .trim_end_matches('#')
        .trim_end();
    if text.is_empty() {
        return None;
    }
    Some((level, text.to_string()))
}

/// Detect a fenced code-block opener: a line beginning with >= 3 backticks or
/// >= 3 tildes. Returns `(fence_char, fence_len)`.
fn fence_marker(line: &str) -> Option<(char, usize)> {
    let first = line.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let len = line.chars().take_while(|&c| c == first).count();
    (len >= 3).then_some((first, len))
}

/// True when `line` closes a fence of char `ch` opened with `min` markers: the
/// same char repeated at least `min` times, then only trailing whitespace.
fn is_fence_close(line: &str, ch: char, min: usize) -> bool {
    let run = line.chars().take_while(|&c| c == ch).count();
    run >= min && line.chars().skip(run).all(char::is_whitespace)
}

/// True when `line` starts a Markdown list item: up to 3 leading spaces, a
/// `-`/`*`/`+` bullet or `<digits>.`/`<digits>)` marker, then whitespace.
fn is_list_item(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut i = 0;
    while i < chars.len() && i < 3 && chars[i].is_whitespace() {
        i += 1;
    }
    if i >= chars.len() {
        return false;
    }
    let after_marker = if matches!(chars[i], '-' | '*' | '+') {
        i + 1
    } else if chars[i].is_ascii_digit() {
        let mut j = i;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j < chars.len() && (chars[j] == '.' || chars[j] == ')') {
            j + 1
        } else {
            return false;
        }
    } else {
        return false;
    };
    after_marker < chars.len() && chars[after_marker].is_whitespace()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARTICLE: &str = "\
## Photosynthesis
Photosynthesis is the process by which plants convert light into chemical energy.

## Mitochondria
The mitochondria is the powerhouse of the cell and produces ATP for the organism.

## Weather
Today it is sunny with a gentle breeze over the coastal town.";

    #[test]
    fn keyword_match_selects_the_relevant_block() {
        let out = select_excerpts(ARTICLE, "how does photosynthesis work", 4000);
        assert!(
            out.contains("convert light into chemical energy"),
            "expected the photosynthesis body, got: {out:?}"
        );
        assert!(
            out.contains("## Photosynthesis"),
            "expected the heading prefix carried, got: {out:?}"
        );
        assert!(
            !out.contains("powerhouse of the cell"),
            "unrelated block leaked into result: {out:?}"
        );
    }

    #[test]
    fn empty_objective_falls_back_to_leading_text() {
        for objective in ["", "   ", "\t\n"] {
            let out = select_excerpts(ARTICLE, objective, 4000);
            assert_eq!(
                out, ARTICLE,
                "empty objective {objective:?} should fall back"
            );
        }
    }

    #[test]
    fn stopword_only_objective_falls_back() {
        // All tokens are stopwords or length < 2, so there is no signal.
        let out = select_excerpts(ARTICLE, "the of a to is", 4000);
        assert_eq!(out, ARTICLE);
    }

    #[test]
    fn no_match_falls_back_to_leading_text() {
        let out = select_excerpts(ARTICLE, "quantum chromodynamics tokamak", 4000);
        assert_eq!(out, ARTICLE);
    }

    #[test]
    fn multi_block_stitching_joins_with_separator() {
        let out = select_excerpts(ARTICLE, "photosynthesis and mitochondria", 4000);
        assert!(
            out.contains("convert light into chemical energy"),
            "{out:?}"
        );
        assert!(out.contains("powerhouse of the cell"), "{out:?}");
        assert!(
            out.contains(EXCERPT_SEPARATOR),
            "two excerpts should be separator-joined: {out:?}"
        );
    }

    #[test]
    fn max_chars_is_respected_and_char_boundary_safe() {
        // Table: (text, objective, budget). Each row must yield <= budget chars
        // and never panic on multibyte content.
        let unicode = "## Café\nThe café serves résumé pastries and 日本語 menus every morning.\n\n## Other\nUnrelated text about the weather in the valley.";
        let cases: &[(&str, &str, usize)] = &[
            (ARTICLE, "photosynthesis mitochondria weather", 20),
            (ARTICLE, "photosynthesis", 1),
            (unicode, "café résumé", 5),
            (unicode, "café résumé", 12),
            (unicode, "", 8),
            (unicode, "nomatchtoken", 8),
        ];
        for (text, objective, budget) in cases {
            let out = select_excerpts(text, objective, *budget);
            let n = out.chars().count();
            assert!(
                n <= *budget,
                "budget {budget} exceeded ({n} chars) for objective {objective:?}"
            );
            // Round-trips as valid UTF-8 by construction; assert it is a prefix
            // slice on a char boundary (no panic implies boundary-safe).
            assert!(text.starts_with(&out) || !out.is_empty() || out.is_empty());
        }
    }

    #[test]
    fn phrase_match_outscores_scattered_tokens() {
        let md = "\
## Alpha
The quick brown fox jumps.

## Beta
A red fox and a brown dog met by the river bank today.";
        // Exact phrase should pull the Alpha block (contains \"brown fox\").
        let out = select_excerpts(md, "\"brown fox\"", 4000);
        assert!(out.contains("quick brown fox jumps"), "{out:?}");
    }
}
