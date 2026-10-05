//! Finding notes: the index behind "Search notes" in the command palette and
//! the note rows ⌘K mixes into its results.
//!
//! The index is one entry per file in the notes folder (live notes, archived
//! notes and files no Session claims alike; trashed notes live in `.trash`
//! and are never listed). It is built off the main thread by the shared
//! To-dos model and refreshed by its directory watcher, reusing entries whose
//! file has not changed, so a keystroke only ranks what is in memory.
//!
//! Ranking: the title is matched fuzzily and weighs most. The body (every
//! block's text: paragraphs, headings, to-dos, table cells, mention labels)
//! is matched word by word, case-insensitively: every query word must appear
//! somewhere in the note. Fuzzy matching every line of a thousand notes on
//! each keystroke does not fit in a frame; whole words in the body do, and
//! are what people type when they remember a phrase.

use std::ops::Range;

use ubra_notes::doc::BlockKind;
use ubra_notes::store::Note;

use crate::fuzzy::{FuzzyMatcher, FuzzyQuery, PreparedText};

/// One searchable block of a note's body.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NoteLine {
    /// The block's index in the note (not counting the title).
    pub block: usize,
    pub text: String,
    lower: String,
    /// Headings and to-dos say what a note is about; a match there ranks
    /// above one in running text.
    pub strong: bool,
}

/// One note in the index.
#[derive(Clone, Debug)]
pub(crate) struct NoteEntry {
    pub id: String,
    /// The workspace whose notes folder holds the file; `None` is global.
    /// Scopes are separate stores, so an id alone does not name a note.
    pub workspace: Option<ubra_proto::ProjectId>,
    pub title: String,
    pub project: Option<String>,
    /// File modification time, milliseconds since the epoch.
    pub modified_ms: u64,
    pub open_todos: usize,
    /// Sessions linked from the note's to-dos, first-seen order.
    pub linked: Vec<String>,
    pub lines: Vec<NoteLine>,
    title_prepared: PreparedText,
    /// Every line, lower-cased and joined, for the cheap "is this word in
    /// the note at all" check.
    body_lower: String,
}

impl NoteEntry {
    pub(crate) fn new(
        id: &str,
        note: &Note,
        modified_ms: u64,
        workspace: Option<ubra_proto::ProjectId>,
    ) -> Self {
        let title = if note.doc.title.trim().is_empty() {
            "Untitled".to_owned()
        } else {
            note.doc.title.trim().to_owned()
        };
        let mut lines = Vec::new();
        for (block, b) in note.doc.blocks.iter().enumerate() {
            // Every kind's text is searchable; an image by its alt text.
            let text = b.text.trim();
            if text.is_empty() {
                continue;
            }
            lines.push(NoteLine {
                block,
                text: text.to_owned(),
                lower: text.to_lowercase(),
                strong: matches!(b.kind, BlockKind::Heading(_) | BlockKind::Todo { .. }),
            });
        }
        let todos = ubra_notes::handoff::todos(note);
        let open_todos = todos
            .iter()
            .filter(|todo| !todo.checked && !todo.text.trim().is_empty())
            .count();
        let mut linked = Vec::new();
        for todo in &todos {
            for session in &todo.sessions {
                if !linked.contains(session) {
                    linked.push(session.clone());
                }
            }
        }
        let body_lower = lines
            .iter()
            .map(|line| line.lower.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        Self {
            id: id.to_owned(),
            workspace,
            title_prepared: PreparedText::new(&title),
            title,
            project: note.project().map(str::to_owned),
            modified_ms,
            open_todos,
            linked,
            lines,
            body_lower,
        }
    }
}

/// Where in a note's body the query matched, ready to draw.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snippet {
    pub block: usize,
    pub text: String,
    /// Byte ranges of `text` to highlight.
    pub ranges: Vec<Range<usize>>,
}

/// One result.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NoteHit {
    /// Index into the entries that were ranked.
    pub entry: usize,
    pub title_ranges: Vec<Range<usize>>,
    /// The best body match, when the body matched.
    pub snippet: Option<Snippet>,
}

/// (tier, score, recency, entry, title ranges): tier 1 for a title match,
/// 0 for a body-only match.
type Scored = (u8, u64, u64, usize, Vec<Range<usize>>);

/// Snippets show this many characters around the match.
const SNIPPET_CHARS: usize = 96;

#[derive(Default)]
pub(crate) struct NotesSearch {
    matcher: Option<FuzzyMatcher>,
}

impl NotesSearch {
    /// Ranks `entries` for `query`: title matches first (best fuzzy score
    /// first), then body-only matches (more words on one line, then a
    /// heading or to-do, first); ties go to the most recently edited. An
    /// empty query lists every note by last edit. At most `limit` results.
    pub(crate) fn rank(
        &mut self,
        entries: &[NoteEntry],
        query: &str,
        limit: usize,
    ) -> Vec<NoteHit> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            let mut order: Vec<usize> = (0..entries.len()).collect();
            order.sort_by_key(|i| std::cmp::Reverse(entries[*i].modified_ms));
            order.truncate(limit);
            return order
                .into_iter()
                .map(|entry| NoteHit {
                    entry,
                    title_ranges: Vec::new(),
                    snippet: None,
                })
                .collect();
        }
        let fuzzy = FuzzyQuery::new(query);
        let matcher = self.matcher.get_or_insert_with(FuzzyMatcher::text);
        // (tier, score, recency, entry, title ranges): tier 1 for a title
        // match, 0 for a body-only match.
        let mut scored: Vec<Scored> = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            let title = fuzzy.highlights(&entry.title_prepared, &entry.title, matcher);
            let in_body = words
                .iter()
                .all(|word| entry.body_lower.contains(word.as_str()));
            match title {
                Some((score, ranges)) => {
                    let score = u64::from(score) * 3 + if in_body { 1 } else { 0 };
                    scored.push((1, score, entry.modified_ms, index, ranges));
                }
                None if in_body || words_in_title_and_body(entry, &words) => {
                    let (on_line, strong) = best_line(entry, &words)
                        .map_or((0, false), |(line, count)| {
                            (count, entry.lines[line].strong)
                        });
                    let score = on_line as u64 * 10 + u64::from(strong) * 5;
                    scored.push((0, score, entry.modified_ms, index, Vec::new()));
                }
                None => {}
            }
        }
        scored.sort_by_key(|s| std::cmp::Reverse((s.0, s.1, s.2)));
        scored.truncate(limit);
        scored
            .into_iter()
            .map(|(_, _, _, entry, title_ranges)| {
                let snippet = best_line(&entries[entry], &words)
                    .map(|(line, _)| snippet(&entries[entry].lines[line], &words));
                NoteHit {
                    entry,
                    title_ranges,
                    snippet,
                }
            })
            .collect()
    }
}

/// Every word is in the note when the title and the body are read together:
/// "launch email" finds "Launch plan" whose body mentions an email.
fn words_in_title_and_body(entry: &NoteEntry, words: &[String]) -> bool {
    let title = entry.title.to_lowercase();
    words
        .iter()
        .all(|word| title.contains(word.as_str()) || entry.body_lower.contains(word.as_str()))
}

/// The line holding the most query words (first such line), and how many.
fn best_line(entry: &NoteEntry, words: &[String]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for (index, line) in entry.lines.iter().enumerate() {
        let count = words
            .iter()
            .filter(|word| line.lower.contains(word.as_str()))
            .count();
        if count > 0 && best.is_none_or(|(_, best)| count > best) {
            best = Some((index, count));
            if count == words.len() {
                break;
            }
        }
    }
    best
}

/// A window of `line` around its first match, every occurrence of every
/// word highlighted. Lower-casing must not change a line's byte length for
/// the ranges to carry over; when it does, matches are found as typed.
fn snippet(line: &NoteLine, words: &[String]) -> Snippet {
    let text = &line.text;
    let lower = if line.lower.len() == text.len() {
        line.lower.clone()
    } else {
        text.clone()
    };
    let first = words
        .iter()
        .filter_map(|word| lower.find(word.as_str()))
        .min()
        .unwrap_or(0);
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let first_char = chars.iter().position(|(i, _)| *i >= first).unwrap_or(0);
    let mut start_char = first_char.saturating_sub(SNIPPET_CHARS / 3);
    // Begin on a whole word: "…email sequence", never "…rding sequence".
    if start_char > 0
        && let Some(space) = chars[start_char..first_char]
            .iter()
            .position(|(_, c)| c.is_whitespace())
    {
        start_char += space + 1;
    }
    let end_char = (start_char + SNIPPET_CHARS).min(chars.len());
    let start = chars.get(start_char).map_or(0, |(i, _)| *i);
    let end = chars.get(end_char).map_or(text.len(), |(i, _)| *i);
    let lead = if start > 0 { "…" } else { "" };
    let tail = if end < text.len() { "…" } else { "" };
    let window = format!("{lead}{}{tail}", &text[start..end]);
    let window_lower = format!("{lead}{}{tail}", &lower[start..end]);
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for word in words {
        let mut from = 0;
        while let Some(at) = window_lower[from..].find(word.as_str()) {
            let begin = from + at;
            ranges.push(begin..begin + word.len());
            from = begin + word.len().max(1);
        }
    }
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    Snippet {
        block: line.block,
        text: window,
        ranges: merged,
    }
}

/// Builds entries for every note in every `store` (the global notes and
/// one entry per workspace scope), reusing `previous` entries whose file
/// did not change. A store that cannot be listed keeps its previous
/// entries; the other scopes still refresh. Runs off the main thread.
pub(crate) fn build_index(
    stores: &[(Option<ubra_proto::ProjectId>, &ubra_notes::store::NoteStore)],
    previous: &[NoteEntry],
) -> Vec<NoteEntry> {
    let previous: std::collections::HashMap<
        (Option<&ubra_proto::ProjectId>, &str, u64),
        &NoteEntry,
    > = previous
        .iter()
        .map(|entry| {
            (
                (
                    entry.workspace.as_ref(),
                    entry.id.as_str(),
                    entry.modified_ms,
                ),
                entry,
            )
        })
        .collect();
    let mut entries = Vec::new();
    for (workspace, store) in stores {
        let Ok(metas) = store.list() else {
            entries.extend(
                previous
                    .values()
                    .filter(|entry| entry.workspace == *workspace)
                    .map(|entry| (*entry).clone()),
            );
            continue;
        };
        for meta in metas {
            if let Some(entry) =
                previous.get(&(workspace.as_ref(), meta.id.as_str(), meta.modified_ms))
            {
                entries.push((*entry).clone());
                continue;
            }
            if let Ok(note) = store.load(&meta.id) {
                entries.push(NoteEntry::new(
                    &meta.id,
                    &note,
                    meta.modified_ms,
                    workspace.clone(),
                ));
            }
        }
    }
    entries.sort_by(|a, b| {
        b.modified_ms
            .cmp(&a.modified_ms)
            .then_with(|| b.id.cmp(&a.id))
            .then_with(|| workspace_key(a).cmp(&workspace_key(b)))
    });
    entries
}

fn workspace_key(entry: &NoteEntry) -> Option<&str> {
    entry.workspace.as_ref().map(|id| id.0.as_str())
}

#[allow(dead_code, reason = "Used by tests and the bench")]
pub(crate) fn entry_from_markdown(id: &str, markdown: &str, modified_ms: u64) -> NoteEntry {
    NoteEntry::new(
        id,
        &ubra_notes::store::parse_note(markdown),
        modified_ms,
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<NoteEntry> {
        vec![
            entry_from_markdown(
                "a",
                "# Q4 launch plan\n\nShip the onboarding email sequence before the pricing test.\n\n- [ ] Draft the launch email\n",
                300,
            ),
            entry_from_markdown("b", "# Groceries\n\n- [ ] Oat milk\n- [x] Bread\n", 500),
            entry_from_markdown(
                "c",
                "# Weekly sync\n\n## Pricing\n\nWe will A/B test the pricing page.\n\n| Channel | CPA |\n|---|---|\n| Search | $14 |\n",
                400,
            ),
            entry_from_markdown("d", "# Launch retro\n\nWhat went well.\n", 100),
        ]
    }

    fn ids(hits: &[NoteHit], entries: &[NoteEntry]) -> Vec<String> {
        hits.iter()
            .map(|hit| entries[hit.entry].id.clone())
            .collect()
    }

    #[test]
    fn an_empty_query_lists_notes_by_last_edit() {
        let entries = entries();
        let hits = NotesSearch::default().rank(&entries, "  ", 10);
        assert_eq!(ids(&hits, &entries), ["b", "c", "a", "d"]);
        assert!(hits.iter().all(|hit| hit.snippet.is_none()));
    }

    #[test]
    fn title_matches_rank_above_body_matches() {
        let entries = entries();
        let hits = NotesSearch::default().rank(&entries, "launch", 10);
        // Both titles with "launch", the one that starts with it first.
        assert_eq!(ids(&hits, &entries), ["d", "a"]);
        assert!(!hits[0].title_ranges.is_empty());
        // "pricing" is in a's body and c's heading: c's heading ranks higher.
        let hits = NotesSearch::default().rank(&entries, "pricing", 10);
        assert_eq!(ids(&hits, &entries), ["c", "a"]);
        let snippet = hits[0].snippet.as_ref().unwrap();
        assert_eq!(snippet.text, "Pricing");
        assert_eq!(snippet.ranges, vec![0..7]);
    }

    #[test]
    fn body_words_can_be_anywhere_in_a_note_and_snippets_highlight_them() {
        let entries = entries();
        let hits = NotesSearch::default().rank(&entries, "onboarding pricing", 10);
        assert_eq!(ids(&hits, &entries), ["a"]);
        let snippet = hits[0].snippet.as_ref().unwrap();
        assert_eq!(snippet.block, 0);
        let marked: Vec<&str> = snippet
            .ranges
            .iter()
            .map(|r| &snippet.text[r.clone()])
            .collect();
        assert_eq!(marked, ["onboarding", "pricing"]);
        // Table cells are searchable.
        let hits = NotesSearch::default().rank(&entries, "$14", 10);
        assert_eq!(ids(&hits, &entries), ["c"]);
        // Fuzzy titles: "grcries" still finds Groceries.
        let hits = NotesSearch::default().rank(&entries, "grcries", 10);
        assert_eq!(ids(&hits, &entries), ["b"]);
        // Nothing matches nothing.
        assert!(
            NotesSearch::default()
                .rank(&entries, "zebra", 10)
                .is_empty()
        );
    }

    #[test]
    fn long_lines_are_cut_around_the_match() {
        let long = format!("# T\n\n{} needle {}\n", "a ".repeat(200), "b ".repeat(200));
        let entries = vec![entry_from_markdown("x", &long, 1)];
        let hits = NotesSearch::default().rank(&entries, "needle", 10);
        let snippet = hits[0].snippet.as_ref().unwrap();
        assert!(snippet.text.starts_with('…') && snippet.text.ends_with('…'));
        assert!(snippet.text.chars().count() <= SNIPPET_CHARS + 2);
        assert_eq!(&snippet.text[snippet.ranges[0].clone()], "needle");
    }

    #[test]
    fn index_keeps_scopes_apart() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("notes");
        let global = ubra_notes::store::NoteStore::open(&root).unwrap();
        let (id, _) = global
            .create(ubra_notes::doc::Document::new("Global", Vec::new()), None)
            .unwrap();
        let scoped = ubra_notes::store::NoteStore::open_workspace(&root, "p_ws").unwrap();
        let (sid, _) = scoped
            .create(ubra_notes::doc::Document::new("Scoped", Vec::new()), None)
            .unwrap();
        // The same file id in both scopes: two notes, not one.
        std::fs::copy(global.path_for(&id).unwrap(), scoped.path_for(&id).unwrap()).unwrap();

        let workspace = Some(ubra_proto::ProjectId::new("p_ws"));
        let stores = vec![(None, &global), (workspace.clone(), &scoped)];
        let entries = build_index(&stores, &[]);
        let keys: Vec<_> = entries
            .iter()
            .map(|e| (e.id.clone(), e.workspace.clone()))
            .collect();
        assert_eq!(keys.len(), 3);
        assert!(keys.contains(&(id.clone(), None)));
        assert!(keys.contains(&(id.clone(), workspace.clone())));
        assert!(keys.contains(&(sid.clone(), workspace.clone())));

        // Rebuilding reuses unchanged entries, whichever scope they are in.
        let again = build_index(&stores, &entries);
        let keys: Vec<_> = again
            .iter()
            .map(|e| (e.id.clone(), e.workspace.clone()))
            .collect();
        assert_eq!(keys.len(), 3);
        assert_eq!(
            again.iter().map(|e| &e.title).collect::<Vec<_>>(),
            entries.iter().map(|e| &e.title).collect::<Vec<_>>()
        );
    }

    #[test]
    fn entries_count_open_todos_and_linked_agents() {
        let entry = entry_from_markdown(
            "t",
            "# Plan\n\n- [ ] Fix flicker [@Codex: fix](ubra://session/s_1)\n- [x] Done [@Claude](ubra://session/s_2)\n- [ ] Ship\n",
            1,
        );
        assert_eq!(entry.open_todos, 2);
        assert_eq!(entry.linked, ["s_1", "s_2"]);
    }

    /// Ranks 1,000 notes of ~40 blocks for typical queries; a search must
    /// stay well under a frame. `UBRA_BENCH_NOTES` overrides the count.
    #[test]
    #[ignore = "search bench; run with --release --ignored --nocapture"]
    fn ranks_a_thousand_notes_well_under_a_frame() {
        let count: usize = std::env::var("UBRA_BENCH_NOTES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000);
        let topics = [
            "launch",
            "pricing",
            "onboarding",
            "retention",
            "hiring",
            "budget",
            "roadmap",
            "churn",
        ];
        let entries: Vec<NoteEntry> = (0..count)
            .map(|i| {
                let topic = topics[i % topics.len()];
                let mut md = format!("# {topic} notes {i}\n\n");
                for j in 0..12 {
                    md.push_str(&format!(
                        "## Section {j}\n\nWe discussed the {topic} work with the team, follow-up {i}-{j} on activation and the weekly funnel review.\n\n- [ ] Ask about {topic} metric {j}\n\n"
                    ));
                }
                md.push_str("| Channel | Spend |\n|---|---|\n| Search | $1,200 |\n");
                entry_from_markdown(&format!("n{i}"), &md, i as u64)
            })
            .collect();
        let mut search = NotesSearch::default();
        for query in [
            "",
            "launch",
            "pricng",
            "weekly funnel",
            "metric 7",
            "zzzz",
            "follow-up 512-3",
        ] {
            let mut times = Vec::new();
            let mut found = 0;
            for _ in 0..20 {
                let start = std::time::Instant::now();
                found = search.rank(&entries, query, 200).len();
                times.push(start.elapsed());
            }
            times.sort();
            eprintln!(
                "notes-search {query:?}: notes={count} results={found} median_ms={:.3} max_ms={:.3}",
                times[times.len() / 2].as_secs_f64() * 1000.0,
                times.last().unwrap().as_secs_f64() * 1000.0
            );
        }
    }
}
