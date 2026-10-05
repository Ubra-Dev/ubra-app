//! Editing a note as Markdown text, the way an agent edits a file.
//!
//! The text is the note's canonical body: [`body`] is exactly what
//! `read_note` returns and what [`edit`] matches against: the writer's
//! output without front matter, title first as a `# ` line. Because it is
//! the writer's own output (normalised list markers, tidy tables, escaped
//! characters), text an agent copies from `read_note` always matches.
//!
//! [`edit`] has the contract of a file Edit tool: `old` must occur exactly
//! once (or every occurrence is replaced with `replace_all`), an empty `new`
//! deletes, and a miss explains what is nearby so the caller can retry.
//! [`replace_section`] swaps everything under one heading.

use crate::doc::{BlockKind, Document};
use crate::markdown::{self, FrontMatter};
use crate::store::Note;

/// The note as editable Markdown: title line, then blocks, no front matter.
pub fn body(note: &Note) -> String {
    markdown::write(&FrontMatter::default(), &note.doc)
}

/// What an edit changed, for the caller to show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edited {
    /// How many places changed.
    pub replacements: usize,
    /// The changed region of the new body with a line of context each side.
    pub excerpt: String,
    /// `old` matched only once table spacing was ignored: Ubra re-aligns
    /// tables after each change, so a row an agent just wrote reads with
    /// different padding.
    pub tolerant: bool,
}

/// Replaces `old` with `new` in the note's [`body`] and re-reads it.
pub fn edit(note: &mut Note, old: &str, new: &str, replace_all: bool) -> Result<Edited, String> {
    if old.is_empty() {
        return Err("old_string is empty: copy the exact text to change from read_note".into());
    }
    if old == new {
        return Err("old_string and new_string are the same, so nothing would change".into());
    }
    let text = body(note);
    let found: Vec<usize> = text.match_indices(old).map(|(at, _)| at).collect();
    match found.len() {
        0 => {
            return match tolerant_match(&text, old) {
                Some(range) => apply(note, &text, range, new, true),
                None => Err(no_match(&text, old)),
            };
        }
        1 => {}
        n if !replace_all => return Err(ambiguous(&text, old, &found, n)),
        _ => {}
    }
    if !replace_all {
        let first = found[0];
        return apply(note, &text, first..first + old.len(), new, false);
    }
    let edited = text.replace(old, new);
    finish(note, edited, found[0], new, found.len(), false)
}

fn apply(
    note: &mut Note,
    text: &str,
    range: std::ops::Range<usize>,
    new: &str,
    tolerant: bool,
) -> Result<Edited, String> {
    let edited = format!("{}{new}{}", &text[..range.start], &text[range.end..]);
    finish(note, edited, range.start, new, 1, tolerant)
}

fn finish(
    note: &mut Note,
    edited: String,
    first: usize,
    new: &str,
    replacements: usize,
    tolerant: bool,
) -> Result<Edited, String> {
    let (_, doc) = markdown::parse(&edited);
    if doc.title.trim().is_empty() && !note.doc.title.trim().is_empty() && !edited.starts_with("# ")
    {
        // The title is the first `# ` line; removing it by accident would
        // turn the first heading into the title. Keep the title instead.
        return Err("that edit would remove the note's title (the first `# ` line); change it rather than delete it".into());
    }
    note.doc = doc;
    let after = body(note);
    Ok(Edited {
        replacements,
        excerpt: around(&after, first, new.len().max(1)),
        tolerant,
    })
}

/// The one place `old` occurs once spacing that Ubra itself changes is
/// ignored: runs of spaces next to `|` and trailing spaces, on table lines
/// only (lines starting with `|`, and lines of `old` containing `|`).
/// `None` unless exactly one place matches.
fn tolerant_match(text: &str, old: &str) -> Option<std::ops::Range<usize>> {
    if !old.contains('|') {
        return None;
    }
    let (haystack, map) = squash_tables(text, |line| line.trim_start().starts_with('|'));
    let (needle, _) = squash_tables(old, |line| line.contains('|'));
    if needle.is_empty() {
        return None;
    }
    let mut hits = haystack.match_indices(&needle).map(|(at, _)| at);
    let start = hits.next()?;
    if hits.next().is_some() {
        return None;
    }
    let end = start + needle.len();
    Some(map[start]..map[end - 1] + 1)
}

/// `text` with spaces dropped next to `|` and at line ends on the lines
/// `is_table` picks, plus each kept byte's index in `text`.
fn squash_tables(text: &str, is_table: impl Fn(&str) -> bool) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut map = Vec::with_capacity(text.len());
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let (content, newline) = match line.strip_suffix('\n') {
            Some(content) => (content, true),
            None => (line, false),
        };
        let table = is_table(content);
        let bytes = content.as_bytes();
        let keep_to = if table {
            content.trim_end_matches(' ').len()
        } else {
            bytes.len()
        };
        for (i, &b) in bytes.iter().enumerate().take(keep_to) {
            if table && b == b' ' {
                // Drop a space that belongs to a run touching a pipe.
                let before = bytes[..i].iter().rev().find(|&&c| c != b' ');
                let after = bytes[i + 1..].iter().find(|&&c| c != b' ');
                if before == Some(&b'|') || after == Some(&b'|') {
                    continue;
                }
            }
            out.push(b as char);
            map.push(offset + i);
        }
        if newline {
            out.push('\n');
            map.push(offset + content.len());
        }
        offset += line.len();
    }
    // Bytes were copied one by one; rebuild UTF-8 faithfully.
    let out = {
        let mut bytes = Vec::with_capacity(map.len());
        for &i in &map {
            bytes.push(text.as_bytes()[i]);
        }
        String::from_utf8(bytes).expect("only ASCII spaces were removed")
    };
    (out, map)
}

/// Replaces everything under `heading` (up to the next heading of the same
/// or a higher level) with `markdown`, keeping the heading itself. `heading`
/// is its text, optionally with its `#` marks to say which level. The note's
/// title counts as the top heading: replacing it swaps the text before the
/// first top-level heading.
pub fn replace_section(
    note: &mut Note,
    heading: &str,
    replacement: &str,
) -> Result<Edited, String> {
    let wanted = heading.trim();
    let level = wanted.chars().take_while(|c| *c == '#').count();
    let name = wanted.trim_start_matches('#').trim();
    if name.is_empty() {
        return Err("heading is empty: pass its text, e.g. \"Status\" or \"## Status\"".into());
    }
    let same = |text: &str| text.trim().eq_ignore_ascii_case(name);
    let blocks = &note.doc.blocks;
    let matches: Vec<(usize, u8)> = blocks
        .iter()
        .enumerate()
        .filter_map(|(i, b)| match b.kind {
            BlockKind::Heading(l)
                if same(&b.text) && (level == 0 || level as u8 == l + 1 || l as usize == level) =>
            {
                Some((i, l))
            }
            _ => None,
        })
        .collect();
    let title_matches = same(&note.doc.title) && (level == 0 || level == 1);
    let (start, end) = match (matches.as_slice(), title_matches) {
        ([(index, level)], false) => {
            let end = blocks[index + 1..]
                .iter()
                .position(|b| matches!(b.kind, BlockKind::Heading(l) if l <= *level))
                .map_or(blocks.len(), |offset| index + 1 + offset);
            (index + 1, end)
        }
        ([], true) => {
            let end = blocks
                .iter()
                .position(|b| matches!(b.kind, BlockKind::Heading(1)))
                .unwrap_or(blocks.len());
            (0, end)
        }
        ([], false) => {
            let headings: Vec<String> = blocks
                .iter()
                .filter_map(|b| match b.kind {
                    BlockKind::Heading(l) => {
                        Some(format!("{} {}", "#".repeat(l as usize + 1), b.text))
                    }
                    _ => None,
                })
                .collect();
            return Err(if headings.is_empty() {
                format!("no heading \"{name}\": this note has no headings; use edit_note instead")
            } else {
                format!(
                    "no heading \"{name}\". The note's headings: {}",
                    headings.join(" · ")
                )
            });
        }
        _ => {
            return Err(format!(
                "\"{name}\" names more than one heading; pass its level too (e.g. \"## {name}\") or use edit_note with surrounding text"
            ));
        }
    };
    let (_, parsed) = markdown::parse(&format!("\n{replacement}"));
    let mut new_blocks: Vec<_> = parsed
        .blocks
        .into_iter()
        .filter(|b| !(b.kind == BlockKind::Paragraph && b.text.is_empty()))
        .collect();
    // The replacement may repeat the heading; it is kept once.
    if start > 0
        && new_blocks
            .first()
            .is_some_and(|b| matches!(b.kind, BlockKind::Heading(_)) && same(&b.text))
    {
        new_blocks.remove(0);
    }
    let anchor = if start == 0 {
        note.doc.title.clone()
    } else {
        note.doc.blocks[start - 1].text.clone()
    };
    let mut blocks = note.doc.blocks.clone();
    blocks.splice(start..end, new_blocks);
    note.doc = Document::new(note.doc.title.clone(), blocks);
    let after = body(note);
    let at = after.find(&anchor).unwrap_or(0);
    Ok(Edited {
        replacements: 1,
        excerpt: around(&after, at, replacement.len().max(anchor.len())),
        tolerant: false,
    })
}

/// The lines covering `start..start + len` plus one line of context each side.
fn around(text: &str, start: usize, len: usize) -> String {
    let start = start.min(text.len());
    let end = (start + len).min(text.len());
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let from = text[..line_start.saturating_sub(1)]
        .rfind('\n')
        .map_or(0, |i| i + 1);
    let line_end = text[end..].find('\n').map_or(text.len(), |i| end + i);
    let to = text[(line_end + 1).min(text.len())..]
        .find('\n')
        .map_or(text.len(), |i| line_end + 1 + i);
    text[from..to].trim_end().to_owned()
}

fn ambiguous(text: &str, old: &str, found: &[usize], count: usize) -> String {
    let places: Vec<String> = found
        .iter()
        .take(5)
        .map(|&at| format!("line {}: {}", line_of(text, at), line_text(text, at)))
        .collect();
    format!(
        "old_string appears {count} times; include more surrounding text so it is unique, or pass replace_all:true to change every one.\n{}{}",
        places.join("\n"),
        if count > 5 { "\n…" } else { "" }
    ) + &format!("\n(old_string: {:?})", shorten(old))
}

fn no_match(text: &str, old: &str) -> String {
    let mut hint = String::new();
    // The most common slip: whitespace or case differs.
    let squash = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    if squash(text).contains(&squash(old)) {
        hint = "It matches if spaces, line breaks, or capital letters are ignored: copy the text exactly as read_note returns it.".into();
    }
    // Show the line that shares the longest start with old_string's first line.
    let first = old
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or(old)
        .trim();
    let best = text
        .lines()
        .enumerate()
        .map(|(i, line)| {
            let common = line
                .trim()
                .chars()
                .zip(first.chars())
                .take_while(|(a, b)| a.eq_ignore_ascii_case(b))
                .count();
            let contains = line.to_lowercase().contains(&first.to_lowercase());
            (if contains { usize::MAX } else { common }, i, line)
        })
        .max_by_key(|(score, i, _)| (*score, usize::MAX - i));
    let nearby = match best {
        Some((score, i, line)) if score >= 4 => {
            format!("\nClosest text, line {}: {}", i + 1, shorten(line))
        }
        _ => String::new(),
    };
    format!(
        "old_string was not found in the note. Read the note again with read_note and copy the text exactly (the note is Markdown, title first).{}{}",
        if hint.is_empty() {
            String::new()
        } else {
            format!("\n{hint}")
        },
        nearby
    )
}

fn line_of(text: &str, at: usize) -> usize {
    text[..at].matches('\n').count() + 1
}

fn line_text(text: &str, at: usize) -> String {
    let start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    shorten(&text[start..end])
}

fn shorten(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= 120 {
        text.to_owned()
    } else {
        format!("{}…", text.chars().take(120).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::parse_note;

    const PRS: &str = "---\nid: n1\n---\n# Release tracker\n\nPRs in flight:\n\n| PR | State |\n| --- | --- |\n| #561 | Done |\n| #562 | Fix |\n\n## Status\n\nWaiting on review.\n\n- [ ] ship\n\n## Notes\n\nKeep this.\n";

    #[test]
    fn a_table_row_changes_from_fix_to_done() {
        let mut note = parse_note(PRS);
        let text = body(&note);
        assert!(text.starts_with("# Release tracker\n"), "{text}");
        assert!(
            !text.contains("id: n1"),
            "front matter is not editable text"
        );
        let row = text
            .lines()
            .find(|l| l.contains("#562"))
            .unwrap()
            .to_owned();
        let done = row.replace("Fix", "Done");
        let edited = edit(&mut note, &row, &done, false).unwrap();
        // Tables are re-tidied, so compare rows with spacing ignored.
        let squash = |s: &str| s.split_whitespace().collect::<String>();
        assert!(
            squash(&edited.excerpt).contains(&squash(&done)),
            "{}",
            edited.excerpt
        );
        let after = body(&note);
        let new_row = after.lines().find(|l| l.contains("#562")).unwrap();
        assert_eq!(squash(new_row), squash(&done));
        // The re-tidied row is what read_note shows next, and it matches.
        edit(
            &mut note,
            new_row,
            &new_row.replace("Done", "Shipped"),
            false,
        )
        .unwrap();
        assert_eq!(note.front.get("id"), Some("n1"), "identity is untouched");
    }

    #[test]
    fn edits_title_and_deletes() {
        let mut note = parse_note(PRS);
        edit(
            &mut note,
            "# Release tracker",
            "# Release 0.1.0 tracker",
            false,
        )
        .unwrap();
        assert_eq!(note.doc.title, "Release 0.1.0 tracker");
        edit(&mut note, "\n\nKeep this.", "", false).unwrap();
        assert!(!body(&note).contains("Keep this"));
        let error = edit(&mut note, "# Release 0.1.0 tracker\n\n", "", false).unwrap_err();
        assert!(error.contains("title"), "{error}");
    }

    #[test]
    fn misses_and_repeats_explain_themselves() {
        let mut note = parse_note(PRS);
        let missing = edit(&mut note, "Waiting on  REVIEW.", "x", false).unwrap_err();
        assert!(missing.contains("not found"), "{missing}");
        assert!(
            missing.contains("spaces, line breaks, or capital letters"),
            "{missing}"
        );
        assert!(missing.contains("Waiting on review."), "{missing}");
        let twice = edit(&mut note, "| #56", "| PR #56", false).unwrap_err();
        assert!(twice.contains("appears 2 times"), "{twice}");
        assert!(twice.contains("replace_all"), "{twice}");
        assert!(twice.contains("#561") && twice.contains("#562"), "{twice}");
        let all = edit(&mut note, "| #56", "| PR #56", true).unwrap();
        assert_eq!(all.replacements, 2);
        assert!(edit(&mut note, "", "x", false).is_err());
    }

    #[test]
    fn a_section_is_replaced_up_to_the_next_heading_of_its_level() {
        let mut note = parse_note(PRS);
        replace_section(&mut note, "Status", "Merged and released.\n\n- [x] ship").unwrap();
        let text = body(&note);
        assert!(
            text.contains(
                "## Status\n\nMerged and released.\n\n- [x] ship\n\n## Notes\n\nKeep this."
            ),
            "{text}"
        );
        assert!(!text.contains("Waiting on review"));
        // Repeating the heading in the replacement does not duplicate it.
        replace_section(&mut note, "## Status", "## Status\n\nAll done.").unwrap();
        assert_eq!(body(&note).matches("## Status").count(), 1);
        // The title's section is the text before the first top-level heading.
        replace_section(&mut note, "Release tracker", "Intro rewritten.").unwrap();
        let text = body(&note);
        assert!(
            text.starts_with("# Release tracker\n\nIntro rewritten.\n\n## Status"),
            "{text}"
        );
    }

    #[test]
    fn missing_and_ambiguous_headings_are_errors() {
        let mut note = parse_note("# T\n\n## A\n\nx\n\n## A\n\ny\n");
        let missing = replace_section(&mut note, "B", "z").unwrap_err();
        assert!(missing.contains("## A"), "{missing}");
        let ambiguous = replace_section(&mut note, "A", "z").unwrap_err();
        assert!(ambiguous.contains("more than one"), "{ambiguous}");
    }

    #[test]
    fn a_second_edit_may_reuse_the_first_new_string_after_re_alignment() {
        let mut note = parse_note(PRS);
        let row = body(&note)
            .lines()
            .find(|l| l.contains("#562"))
            .unwrap()
            .to_owned();
        // First edit: the agent's own spacing, which Ubra then re-aligns.
        let first_new = "| #562 | Done |";
        let first = edit(&mut note, &row, first_new, false).unwrap();
        assert!(!first.tolerant);
        // Second edit: the previous new_string no longer matches exactly.
        let second = edit(&mut note, first_new, "| #562 | Shipped |", false).unwrap();
        assert!(second.tolerant, "matched ignoring table spacing");
        let text = body(&note);
        assert!(
            text.contains("#562") && text.contains("Shipped") && !text.contains("Done  |\n| #562")
        );
        let row = text.lines().find(|l| l.contains("#562")).unwrap();
        assert_eq!(row.split_whitespace().collect::<String>(), "|#562|Shipped|");
        // Exact first: an exact match is never reported as tolerant.
        let exact = edit(&mut note, "Waiting on review.", "Waiting on QA.", false).unwrap();
        assert!(!exact.tolerant);
        // Tolerance stays inside tables, and never picks among several rows.
        assert!(edit(&mut note, "Waiting  on QA.", "x", false).is_err());
        let mut two =
            parse_note("# T\n\n| PR | State |\n| --- | --- |\n| #1 | Done |\n| #2 | Done |\n");
        let error = edit(&mut two, "|Done|", "| x |", false).unwrap_err();
        assert!(error.contains("not found"), "{error}");
    }
}
