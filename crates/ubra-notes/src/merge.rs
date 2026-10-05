//! Three-way merge of a note: the person's unsaved typing with a write made
//! outside the editor (an agent appending, the CLI ticking a to-do).
//!
//! Blocks are the unit. Each block of the last saved note is kept, edited, or
//! removed on each side, and blocks are inserted between them. A block only
//! one side changed takes that change; blocks either side inserted are all
//! kept, the person's first. A block both sides edited keeps the person's
//! text with the outside checkbox, or with a suffix the outside write
//! appended (a linked session chip); any other outside edit of that same
//! block yields to the person's and stays in version history. Rewrites that
//! share nothing are not the same block, so both survive.

use crate::doc::{Block, BlockKind, Document};

/// A merged document and where each of the person's blocks went.
#[derive(Clone, Debug)]
pub struct Merged {
    pub doc: Document,
    /// For each block of `mine` (by index), its index in `doc`, if it
    /// survived the merge.
    pub mine_to_merged: Vec<Option<usize>>,
}

/// Merges `mine` (the editor) and `theirs` (the file now) against `base`
/// (the file the editor last loaded or saved).
pub fn merge3(base: &Document, mine: &Document, theirs: &Document) -> Merged {
    let base_blocks = content(&base.blocks);
    // The editor always keeps an empty paragraph to type into at the end;
    // it is not content, so it is merged as if absent.
    let mine_blocks = content(&mine.blocks);
    let their_blocks = content(&theirs.blocks);
    let ours = align(base_blocks, mine_blocks);
    let other = align(base_blocks, their_blocks);

    let mut out: Vec<Block> = Vec::new();
    let mut mine_to_merged = vec![None; mine.blocks.len()];
    let mut keep_mine = |index: usize, block: Block, out: &mut Vec<Block>| {
        mine_to_merged[index] = Some(out.len());
        out.push(block);
    };
    for gap in 0..=base_blocks.len() {
        // What each side inserted here: the person's first, then outside
        // additions the person did not also type.
        for &index in &ours.inserted[gap] {
            keep_mine(index, mine_blocks[index].clone(), &mut out);
        }
        for &index in &other.inserted[gap] {
            let block = &their_blocks[index];
            if !ours.inserted[gap]
                .iter()
                .any(|&m| equal(&mine_blocks[m], block))
            {
                out.push(block.clone());
            }
        }
        let Some(base_block) = base_blocks.get(gap) else {
            break;
        };
        match (ours.slots[gap], other.slots[gap]) {
            (Slot::Kept(m), Slot::Kept(_)) => keep_mine(m, mine_blocks[m].clone(), &mut out),
            // Untouched by the person, changed outside: the same block, so
            // a caret in it stays in it.
            (Slot::Kept(m), Slot::Changed(t)) => keep_mine(m, their_blocks[t].clone(), &mut out),
            (Slot::Kept(_), Slot::Deleted) => {}
            (Slot::Changed(m), Slot::Kept(_) | Slot::Deleted) => {
                keep_mine(m, mine_blocks[m].clone(), &mut out)
            }
            (Slot::Changed(m), Slot::Changed(t)) => {
                let merged = merge_block(base_block, &mine_blocks[m], &their_blocks[t]);
                keep_mine(m, merged, &mut out);
            }
            // The person deleted it; an outside change to it survives.
            (Slot::Deleted, Slot::Changed(t)) => out.push(their_blocks[t].clone()),
            (Slot::Deleted, _) => {}
        }
    }
    let title = if mine.title == base.title {
        theirs.title.clone()
    } else {
        mine.title.clone()
    };
    Merged {
        doc: Document::new(title, out),
        mine_to_merged,
    }
}

#[derive(Clone, Copy, Debug)]
enum Slot {
    /// Unchanged, at this index of the other side.
    Kept(usize),
    /// Edited in place into this index.
    Changed(usize),
    Deleted,
}

/// How one side changed `base`: each base block's fate, and the blocks
/// inserted before each base block (`inserted[base.len()]` is the end).
struct Alignment {
    slots: Vec<Slot>,
    inserted: Vec<Vec<usize>>,
}

fn align(base: &[Block], other: &[Block]) -> Alignment {
    let matched = matching(base, other);
    let mut slots = vec![Slot::Deleted; base.len()];
    let mut inserted = vec![Vec::new(); base.len() + 1];
    let (mut j, mut k) = (0, 0);
    loop {
        // The next matched base block closes a run of unmatched blocks on
        // both sides; paired positionally, those are edits in place.
        let next = (j..base.len()).find(|&i| matched[i].is_some());
        let (j_end, k_end) = match next {
            Some(i) => (i, matched[i].unwrap()),
            None => (base.len(), other.len()),
        };
        // Within the run, a base block pairs with the next block on the
        // other side that looks like its edit; blocks skipped on the way
        // were inserted before it.
        let mut cursor = k;
        for index in j..j_end {
            if let Some(found) = (cursor..k_end).find(|&x| similar(&base[index], &other[x])) {
                inserted[index].extend(cursor..found);
                slots[index] = Slot::Changed(found);
                cursor = found + 1;
            }
        }
        inserted[j_end].extend(cursor..k_end);
        let Some(i) = next else { break };
        slots[i] = Slot::Kept(k_end);
        (j, k) = (i + 1, k_end + 1);
    }
    Alignment { slots, inserted }
}

/// One block both sides changed.
fn merge_block(base: &Block, mine: &Block, theirs: &Block) -> Block {
    let mut merged = mine.clone();
    if mine.kind == base.kind {
        merged.kind = theirs.kind;
    }
    if mine.indent == base.indent {
        merged.indent = theirs.indent;
    }
    if mine.text == base.text && mine.marks == base.marks {
        merged.text = theirs.text.clone();
        merged.marks = theirs.marks.clone();
    } else if theirs.text.len() > base.text.len()
        && theirs.text.starts_with(&base.text)
        && theirs
            .marks
            .iter()
            .all(|mark| mark.range.start >= base.text.len() || base.marks.contains(mark))
    {
        // The outside write appended to this block (a session chip on a
        // to-do): carry the appended tail onto the person's text.
        let shift = merged.text.len();
        let start = base.text.len();
        merged.text.push_str(&theirs.text[start..]);
        for mark in theirs.marks.iter().filter(|mark| mark.range.start >= start) {
            let range = mark.range.start - start + shift..mark.range.end - start + shift;
            merged.add_mark(range, mark.style.clone());
        }
    }
    merged
}

/// Blocks without the editor's trailing empty paragraphs.
fn content(blocks: &[Block]) -> &[Block] {
    let mut end = blocks.len();
    while end > 0 && blocks[end - 1].kind == BlockKind::Paragraph && blocks[end - 1].text.is_empty()
    {
        end -= 1;
    }
    &blocks[..end]
}

/// Whether `edited` looks like an edit of `original`: the same kind of block
/// (a ticked to-do is still a to-do) whose texts share at least half of the
/// shorter one as a common start.
fn similar(original: &Block, edited: &Block) -> bool {
    let kind = |b: &Block| match b.kind {
        BlockKind::Todo { .. } => BlockKind::Todo { checked: false },
        kind => kind,
    };
    if kind(original) != kind(edited) {
        return false;
    }
    let shorter = original.text.len().min(edited.text.len());
    let common = original
        .text
        .bytes()
        .zip(edited.text.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    shorter == 0 || common * 2 >= shorter
}

fn equal(a: &Block, b: &Block) -> bool {
    a.kind == b.kind && a.indent == b.indent && a.text == b.text && a.marks == b.marks
}

/// For each block of `base`, the index of the block it matches in `other`
/// (a longest common subsequence). Shared prefixes and suffixes are matched
/// directly so typical notes never reach the quadratic middle.
fn matching(base: &[Block], other: &[Block]) -> Vec<Option<usize>> {
    let mut result = vec![None; base.len()];
    let mut prefix = 0;
    while prefix < base.len() && prefix < other.len() && equal(&base[prefix], &other[prefix]) {
        result[prefix] = Some(prefix);
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < base.len() - prefix
        && suffix < other.len() - prefix
        && equal(
            &base[base.len() - 1 - suffix],
            &other[other.len() - 1 - suffix],
        )
    {
        result[base.len() - 1 - suffix] = Some(other.len() - 1 - suffix);
        suffix += 1;
    }
    let a = &base[prefix..base.len() - suffix];
    let b = &other[prefix..other.len() - suffix];
    // Beyond this the middle is matched greedily rather than optimally.
    const MAX_CELLS: usize = 4_000_000;
    if a.len().saturating_mul(b.len()) > MAX_CELLS {
        let mut next = 0;
        for (i, block) in a.iter().enumerate() {
            if let Some(found) = (next..b.len()).find(|&j| equal(block, &b[j])) {
                result[prefix + i] = Some(prefix + found);
                next = found + 1;
            }
        }
        return result;
    }
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[at(i, j)] = if equal(&a[i], &b[j]) {
                lcs[at(i + 1, j + 1)] + 1
            } else {
                lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if equal(&a[i], &b[j]) {
            result[prefix + i] = Some(prefix + j);
            i += 1;
            j += 1;
        } else if lcs[at(i + 1, j)] >= lcs[at(i, j + 1)] {
            i += 1;
        } else {
            j += 1;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown;

    fn doc(source: &str) -> Document {
        markdown::parse(source).1
    }

    fn text(merged: &Merged) -> String {
        markdown::write(&Default::default(), &merged.doc)
    }

    const BASE: &str = "# Launch\n\nIntro.\n\n- [ ] call the venue\n- [ ] book flights\n";

    #[test]
    fn an_outside_append_lands_after_the_persons_typing() {
        let mine = doc(
            "# Launch\n\nIntro, now longer.\n\n- [ ] call the venue\n- [ ] book flights\n\nA new thought\n",
        );
        let theirs = doc(&format!("{BASE}\n## Updates\n\n- agent: venue confirmed\n"));
        let merged = merge3(&doc(BASE), &mine, &theirs);
        assert_eq!(
            text(&merged),
            "# Launch\n\nIntro, now longer.\n\n- [ ] call the venue\n- [ ] book flights\n\nA new thought\n\n## Updates\n\n- agent: venue confirmed\n"
        );
    }

    #[test]
    fn a_tick_and_a_chip_combine_with_typing_in_the_same_todo() {
        let mine = doc("# Launch\n\nIntro.\n\n- [ ] call the venue today\n- [ ] book flights\n");
        let theirs = doc(
            "# Launch\n\nIntro.\n\n- [x] call the venue [@Codex](ubra://session/s_1)\n- [ ] book flights\n",
        );
        let merged = merge3(&doc(BASE), &mine, &theirs);
        assert_eq!(
            text(&merged),
            "# Launch\n\nIntro.\n\n- [x] call the venue today [@Codex](ubra://session/s_1)\n- [ ] book flights\n"
        );
    }

    #[test]
    fn one_sided_changes_pass_through_and_blocks_are_mapped() {
        let mine = doc("# Launch\n\nIntro.\n\nMine\n\n- [ ] call the venue\n- [ ] book flights\n");
        let theirs = doc("# Launch\n\nIntro.\n\n- [ ] call the venue\n- [x] book flights\n");
        let merged = merge3(&doc(BASE), &mine, &theirs);
        assert_eq!(
            text(&merged),
            "# Launch\n\nIntro.\n\nMine\n\n- [ ] call the venue\n- [x] book flights\n"
        );
        // "Mine" is mine[1] and stays at merged[1]; the to-do ticked outside
        // is still the same block, so a caret in it stays there.
        assert_eq!(merged.mine_to_merged[1], Some(1));
        assert_eq!(merged.mine_to_merged[3], Some(3));
        assert_eq!(merged.doc.blocks[3].kind, BlockKind::Todo { checked: true });
    }

    #[test]
    fn competing_rewrites_keep_both_the_persons_first() {
        let mine = doc("# Launch\n\nMy intro.\n\n- [ ] call the venue\n- [ ] book flights\n");
        let theirs = doc("# Launch\n\nTheir intro.\n\n- [ ] call the venue\n- [ ] book flights\n");
        let merged = merge3(&doc(BASE), &mine, &theirs);
        assert!(
            text(&merged).contains("My intro.\n\nTheir intro."),
            "{}",
            text(&merged)
        );
        // Two edits of the same line: the person's wins, the other is in history.
        let mine = doc("# Launch\n\nIntro. Mine.\n\n- [ ] call the venue\n- [ ] book flights\n");
        let theirs =
            doc("# Launch\n\nIntro! Theirs.\n\n- [ ] call the venue\n- [ ] book flights\n");
        let merged = merge3(&doc(BASE), &mine, &theirs);
        assert!(text(&merged).contains("Intro. Mine.") && !text(&merged).contains("Theirs"));
    }

    #[test]
    fn the_trailing_empty_paragraph_is_not_content() {
        let mut mine = doc(BASE);
        mine.blocks.push(Block::new(0, BlockKind::Paragraph, ""));
        let theirs = doc(&format!("{BASE}\n- [ ] print badges\n"));
        let merged = merge3(&doc(BASE), &mine, &theirs);
        assert_eq!(text(&merged), format!("{BASE}- [ ] print badges\n"));
    }

    #[test]
    fn random_edits_keep_every_side_they_should() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let block = |n: u64| {
            let kind = if n.is_multiple_of(3) {
                BlockKind::Todo { checked: false }
            } else {
                BlockKind::Paragraph
            };
            Block::new(0, kind, format!("line {n}"))
        };
        for round in 0..2000 {
            let base: Vec<Block> = (0..next(8)).map(|_| block(next(1000))).collect();
            // Mine: edit some blocks, insert some new ones.
            let mut mine = base.clone();
            for _ in 0..next(3) {
                if !mine.is_empty() {
                    let at = next(mine.len() as u64) as usize;
                    mine[at].text.push_str(" typed");
                }
            }
            for _ in 0..next(2) {
                let at = next(mine.len() as u64 + 1) as usize;
                mine.insert(
                    at,
                    Block::new(0, BlockKind::Paragraph, format!("mine {round}")),
                );
            }
            // Theirs: additive only: tick to-dos, append blocks.
            let mut theirs = base.clone();
            for b in &mut theirs {
                if b.kind == (BlockKind::Todo { checked: false }) && next(2) == 0 {
                    b.kind = BlockKind::Todo { checked: true };
                }
            }
            let added: Vec<String> = (0..next(3)).map(|n| format!("agent {round}.{n}")).collect();
            theirs.extend(
                added
                    .iter()
                    .map(|t| Block::new(0, BlockKind::Bullet, t.clone())),
            );

            let (b, m, t) = (
                Document::new("T", base),
                Document::new("T", mine),
                Document::new("T", theirs),
            );
            let merged = merge3(&b, &m, &b);
            assert!(
                merged
                    .doc
                    .blocks
                    .iter()
                    .map(|x| &x.text)
                    .eq(m.blocks.iter().map(|x| &x.text)),
                "round {round}: no outside change"
            );
            let merged = merge3(&b, &b, &t);
            assert!(
                merged
                    .doc
                    .blocks
                    .iter()
                    .map(|x| &x.text)
                    .eq(t.blocks.iter().map(|x| &x.text)),
                "round {round}: no typing"
            );
            let merged = merge3(&b, &m, &t);
            for (index, block) in m.blocks.iter().enumerate() {
                // An empty paragraph is the editor's typing slot, not content.
                if block.text.is_empty() {
                    continue;
                }
                let at = merged.mine_to_merged[index].expect("the person's blocks are kept");
                assert_eq!(merged.doc.blocks[at].text, block.text, "round {round}");
            }
            for text in &added {
                assert!(
                    merged.doc.blocks.iter().any(|x| &x.text == text),
                    "round {round}: {text} lost"
                );
            }
            let ticked = t
                .blocks
                .iter()
                .filter(|x| x.kind == BlockKind::Todo { checked: true })
                .count();
            let merged_ticked = merged
                .doc
                .blocks
                .iter()
                .filter(|x| x.kind == BlockKind::Todo { checked: true })
                .count();
            if merged_ticked != ticked {
                let show = |d: &Document| {
                    d.blocks
                        .iter()
                        .map(|x| format!("{:?}:{}", x.kind, x.text))
                        .collect::<Vec<_>>()
                };
                panic!(
                    "round {round}: ticks\nbase {:?}\nmine {:?}\ntheirs {:?}\nmerged {:?}",
                    show(&b),
                    show(&m),
                    show(&t),
                    show(&merged.doc)
                );
            }
        }
    }
}
