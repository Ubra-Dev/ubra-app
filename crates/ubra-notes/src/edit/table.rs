//! Tables in the editor: a table is a run of cell blocks, row by row, so the
//! caret, selection, IME, marks and undo work in cells exactly as in any
//! other block. These operations keep that run whole: complete rows, one
//! header row, and every cell labelled with its column, the column count and
//! its column's alignment.

use std::ops::Range;

use super::{EditKind, Editor, Pos, Selection};
use crate::doc::{Align, Block, BlockKind, Cell, MAX_TABLE_COLS, table_range};

/// Where a cell sits in its table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TablePos {
    /// The table's blocks.
    pub range: Range<usize>,
    pub row: usize,
    pub col: usize,
    pub rows: usize,
    pub cols: usize,
}

impl TablePos {
    /// The block index of (`row`, `col`).
    pub fn index(&self, row: usize, col: usize) -> usize {
        self.range.start + row * self.cols + col
    }
}

impl Editor {
    /// The table and cell at block `index`, if it is a cell.
    pub fn table_pos(&self, index: usize) -> Option<TablePos> {
        let cell = self.blocks.get(index)?.kind.cell()?;
        let range = table_range(&self.blocks, index)?;
        let cols = usize::from(cell.cols.max(1));
        let rows = range.len().div_ceil(cols);
        let offset = index - range.start;
        Some(TablePos {
            range,
            row: offset / cols,
            col: offset % cols,
            rows,
            cols,
        })
    }

    /// The table holding the caret.
    pub fn caret_table(&self) -> Option<TablePos> {
        self.table_pos(self.selection.head.block)
    }

    fn aligns(&self, table: &TablePos) -> Vec<Align> {
        (0..table.cols)
            .map(|col| {
                self.blocks[table.range.start + col]
                    .kind
                    .cell()
                    .map_or(Align::None, |c| c.align)
            })
            .collect()
    }

    /// Rewrites every cell's label in `range` for `cols` columns.
    fn relabel(&mut self, range: Range<usize>, cols: usize, aligns: &[Align]) {
        for (i, index) in range.enumerate() {
            let col = i % cols;
            self.blocks[index].kind = BlockKind::Cell(Cell {
                col: col as u16,
                cols: cols as u16,
                align: aligns.get(col).copied().unwrap_or_default(),
                header: i < cols,
            });
        }
    }

    fn new_cell(&mut self, text: &str) -> Block {
        let id = self.fresh_id();
        Block::cell(
            id,
            Cell {
                col: 0,
                cols: 1,
                align: Align::None,
                header: false,
            },
            text,
        )
    }

    fn place_caret(&mut self, index: usize, at_end: bool) {
        let offset = if at_end {
            self.blocks[index].text.len()
        } else {
            0
        };
        self.selection = Selection::caret(Pos::new(index, offset));
        self.pending = None;
    }

    /// Inserts a table of `rows` (the first is the header) below the caret's
    /// block, or in place of an empty line, and puts the caret in its first
    /// cell. Cell text is single-line.
    pub fn insert_table(&mut self, rows: &[Vec<String>], now_ms: u64) {
        let cols = rows
            .iter()
            .map(Vec::len)
            .max()
            .unwrap_or(0)
            .clamp(1, MAX_TABLE_COLS);
        if rows.is_empty() {
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        self.delete_selection_inner();
        let mut cells = Vec::with_capacity(rows.len() * cols);
        for row in rows {
            for col in 0..cols {
                let text = row
                    .get(col)
                    .map_or("", String::as_str)
                    .replace(['\n', '\r'], " ");
                let cell = self.new_cell(text.trim());
                cells.push(cell);
            }
        }
        let at = self.block_slot_after_caret();
        let len = cells.len();
        self.blocks.splice(at..at, cells);
        self.relabel(at..at + len, cols, &vec![Align::None; cols]);
        if self.blocks.get(at + len).is_none_or(|b| b.kind.is_cell()) {
            let id = self.fresh_id();
            self.blocks
                .insert(at + len, Block::new(id, BlockKind::Paragraph, ""));
        }
        self.place_caret(at, true);
        self.changed();
    }

    /// An empty `rows` × `cols` table (`/table`).
    pub fn insert_empty_table(&mut self, rows: usize, cols: usize, now_ms: u64) {
        let rows = vec![vec![String::new(); cols.max(1)]; rows.max(1)];
        self.insert_table(&rows, now_ms);
    }

    /// Where a new block goes: replacing an empty line, else after the
    /// caret's block, else after the table the caret is in.
    fn block_slot_after_caret(&mut self) -> usize {
        let head = self.selection.head.block;
        if let Some(table) = self.table_pos(head) {
            return table.range.end;
        }
        let block = &self.blocks[head];
        if head > 0 && block.kind == BlockKind::Paragraph && block.text.is_empty() {
            self.blocks.remove(head);
            return head;
        }
        head + 1
    }

    /// Where pasted blocks go: the same slot a new table takes.
    pub(super) fn block_slot_after_caret_for_paste(&mut self) -> usize {
        self.block_slot_after_caret()
    }

    /// Adds a row above or below the caret's, caret to the same column.
    pub fn table_add_row(&mut self, below: bool, now_ms: u64) -> bool {
        let Some(table) = self.caret_table() else {
            return false;
        };
        self.checkpoint(EditKind::Other, now_ms);
        self.add_row_inner(&table, below);
        self.changed();
        true
    }

    pub(super) fn add_row_inner(&mut self, table: &TablePos, below: bool) {
        let row = if below { table.row + 1 } else { table.row };
        let at = table.range.start + row * table.cols;
        let aligns = self.aligns(table);
        let cells: Vec<Block> = (0..table.cols).map(|_| self.new_cell("")).collect();
        self.blocks.splice(at..at, cells);
        let range = table.range.start..table.range.end + table.cols;
        self.relabel(range, table.cols, &aligns);
        self.place_caret(at + table.col, false);
    }

    /// Adds a column left or right of the caret's, caret into it.
    pub fn table_add_col(&mut self, right: bool, now_ms: u64) -> bool {
        let Some(table) = self.caret_table() else {
            return false;
        };
        if table.cols >= MAX_TABLE_COLS {
            return false;
        }
        self.checkpoint(EditKind::Other, now_ms);
        let mut aligns = self.aligns(&table);
        let col = if right { table.col + 1 } else { table.col };
        aligns.insert(col, Align::None);
        for row in (0..table.rows).rev() {
            let at = table.range.start + row * table.cols + col;
            let cell = self.new_cell("");
            self.blocks.insert(at, cell);
        }
        let cols = table.cols + 1;
        let range = table.range.start..table.range.end + table.rows;
        self.relabel(range.clone(), cols, &aligns);
        self.place_caret(range.start + table.row * cols + col, false);
        self.changed();
        true
    }

    /// Deletes the caret's row; the last row takes the table with it.
    pub fn table_delete_row(&mut self, now_ms: u64) -> bool {
        let Some(table) = self.caret_table() else {
            return false;
        };
        if table.rows == 1 {
            return self.table_delete(now_ms);
        }
        self.checkpoint(EditKind::Other, now_ms);
        let at = table.index(table.row, 0);
        self.blocks.drain(at..at + table.cols);
        let aligns = self.aligns_from(table.range.start, table.cols);
        let range = table.range.start..table.range.end - table.cols;
        self.relabel(range.clone(), table.cols, &aligns);
        let row = table.row.min(table.rows - 2);
        self.place_caret(range.start + row * table.cols + table.col, true);
        self.changed();
        true
    }

    /// Deletes the caret's column; the last column takes the table with it.
    pub fn table_delete_col(&mut self, now_ms: u64) -> bool {
        let Some(table) = self.caret_table() else {
            return false;
        };
        if table.cols == 1 {
            return self.table_delete(now_ms);
        }
        self.checkpoint(EditKind::Other, now_ms);
        let mut aligns = self.aligns(&table);
        aligns.remove(table.col);
        for row in (0..table.rows).rev() {
            self.blocks.remove(table.index(row, table.col));
        }
        let cols = table.cols - 1;
        let range = table.range.start..table.range.end - table.rows;
        self.relabel(range.clone(), cols, &aligns);
        let col = table.col.min(cols - 1);
        self.place_caret(range.start + table.row * cols + col, true);
        self.changed();
        true
    }

    fn aligns_from(&self, start: usize, cols: usize) -> Vec<Align> {
        (0..cols)
            .map(|c| {
                self.blocks
                    .get(start + c)
                    .and_then(|b| b.kind.cell())
                    .map_or(Align::None, |c| c.align)
            })
            .collect()
    }

    /// Sets the alignment of the caret's column.
    pub fn table_set_align(&mut self, align: Align, now_ms: u64) -> bool {
        let Some(table) = self.caret_table() else {
            return false;
        };
        if self.aligns(&table)[table.col] == align {
            return false;
        }
        self.checkpoint(EditKind::Other, now_ms);
        let mut aligns = self.aligns(&table);
        aligns[table.col] = align;
        self.relabel(table.range.clone(), table.cols, &aligns);
        self.changed();
        true
    }

    /// Removes the caret's table; the caret goes to the line after it.
    pub fn table_delete(&mut self, now_ms: u64) -> bool {
        let Some(table) = self.caret_table() else {
            return false;
        };
        self.checkpoint(EditKind::Other, now_ms);
        self.blocks.drain(table.range.clone());
        let at = table.range.start.min(self.blocks.len() - 1);
        if self.blocks[at].kind == BlockKind::Title {
            let id = self.fresh_id();
            self.blocks
                .insert(at + 1, Block::new(id, BlockKind::Paragraph, ""));
            self.place_caret(at + 1, false);
        } else {
            self.place_caret(at, false);
        }
        self.changed();
        true
    }

    /// Tab / ⇧Tab in a table: the next or previous cell, caret at its end.
    /// Tab in the last cell adds a row. Returns false outside a table.
    pub fn table_tab(&mut self, forward: bool, now_ms: u64) -> bool {
        let Some(table) = self.caret_table() else {
            return false;
        };
        let here = table.index(table.row, table.col);
        if forward {
            if here + 1 < table.range.end {
                self.set_caret(Pos::new(here + 1, self.blocks[here + 1].text.len()));
            } else {
                self.checkpoint(EditKind::Other, now_ms);
                let last = TablePos {
                    col: 0,
                    ..table.clone()
                };
                self.add_row_inner(&last, true);
                self.changed();
            }
        } else if here > table.range.start {
            self.set_caret(Pos::new(here - 1, self.blocks[here - 1].text.len()));
        }
        true
    }

    /// Pastes `rows` from a spreadsheet or a Markdown table: into the table
    /// at the caret, starting at its cell and growing the table to fit, or
    /// as a new table whose first row is the header.
    pub fn paste_rows(&mut self, rows: &[Vec<String>], now_ms: u64) {
        let Some(table) = self.caret_table() else {
            self.insert_table(rows, now_ms);
            return;
        };
        if rows.is_empty() {
            return;
        }
        self.checkpoint(EditKind::Other, now_ms);
        let width = rows.iter().map(Vec::len).max().unwrap_or(1);
        let mut table = table;
        let need_cols = (table.col + width).min(MAX_TABLE_COLS);
        while table.cols < need_cols {
            let mut aligns = self.aligns(&table);
            aligns.push(Align::None);
            for row in (0..table.rows).rev() {
                let at = table.range.start + row * table.cols + table.cols;
                let cell = self.new_cell("");
                self.blocks.insert(at, cell);
            }
            let cols = table.cols + 1;
            let range = table.range.start..table.range.end + table.rows;
            self.relabel(range.clone(), cols, &aligns);
            table = TablePos {
                range,
                cols,
                ..table
            };
        }
        while table.rows < table.row + rows.len() {
            let last = TablePos {
                row: table.rows - 1,
                ..table.clone()
            };
            self.add_row_inner(&last, true);
            table = TablePos {
                range: table.range.start..table.range.end + table.cols,
                rows: table.rows + 1,
                ..table
            };
        }
        let mut last = table.index(table.row, table.col);
        for (r, row) in rows.iter().enumerate() {
            for (c, text) in row.iter().enumerate() {
                let col = table.col + c;
                if col >= table.cols {
                    break;
                }
                let index = table.index(table.row + r, col);
                let block = &mut self.blocks[index];
                let len = block.text.len();
                block.replace(0..len, text.replace(['\n', '\r'], " ").trim(), &[]);
                last = index;
            }
        }
        self.place_caret(last, true);
        self.changed();
    }

    /// The selected cells as tab-separated values, when the selection lies
    /// in one table: the rectangle its two ends span, ready for a
    /// spreadsheet. `None` for a caret or a selection that leaves the table.
    pub fn selection_tsv(&self) -> Option<String> {
        if self.selection.is_collapsed() {
            return None;
        }
        let a = self.table_pos(self.selection.anchor.block)?;
        let b = self.table_pos(self.selection.head.block)?;
        if a.range != b.range {
            return None;
        }
        let (r0, r1) = (a.row.min(b.row), a.row.max(b.row));
        let (c0, c1) = (a.col.min(b.col), a.col.max(b.col));
        let mut out = String::new();
        for row in r0..=r1 {
            if row > r0 {
                out.push('\n');
            }
            for col in c0..=c1 {
                if col > c0 {
                    out.push('\t');
                }
                out.push_str(&tsv_field(&self.blocks[a.index(row, col)].text));
            }
        }
        Some(out)
    }
}

fn tsv_field(text: &str) -> String {
    if text.contains(['\t', '\n', '"']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_owned()
    }
}

/// How pasted rows arrived, for counts-only telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowsSource {
    Tsv,
    Csv,
}

impl RowsSource {
    pub fn name(self) -> &'static str {
        match self {
            Self::Tsv => "tsv",
            Self::Csv => "csv",
        }
    }
}

/// Reads spreadsheet text off the pasteboard: tab-separated values (Google
/// Sheets, Excel, Numbers), or comma-separated values that are unmistakably
/// a table (two or more lines with the same number of fields, at least
/// two). Quoted fields may hold tabs, commas, newlines and `""`. Prose with
/// commas stays prose.
pub fn parse_rows(text: &str) -> Option<(RowsSource, Vec<Vec<String>>)> {
    let text = text.trim_end_matches(['\n', '\r']);
    if text.is_empty() {
        return None;
    }
    if text.contains('\t') {
        let rows = split_delimited(text, '\t');
        if rows.iter().any(|row| row.len() >= 2) {
            return Some((RowsSource::Tsv, pad(rows)));
        }
        return None;
    }
    if !text.contains(',') {
        return None;
    }
    let rows = split_delimited(text, ',');
    let width = rows.first().map_or(0, Vec::len);
    let consistent = rows.len() >= 2 && width >= 2 && rows.iter().all(|row| row.len() == width);
    consistent.then_some((RowsSource::Csv, rows))
}

fn pad(mut rows: Vec<Vec<String>>) -> Vec<Vec<String>> {
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    for row in &mut rows {
        row.resize(width, String::new());
    }
    rows
}

fn split_delimited(text: &str, separator: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = text.chars().peekable();
    let mut quoted = false;
    let mut at_field_start = true;
    while let Some(ch) = chars.next() {
        if quoted {
            match ch {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(ch),
            }
            continue;
        }
        match ch {
            '"' if at_field_start => {
                quoted = true;
                at_field_start = false;
            }
            c if c == separator => {
                row.push(std::mem::take(&mut field));
                at_field_start = true;
            }
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                at_field_start = true;
            }
            _ => {
                field.push(ch);
                at_field_start = false;
            }
        }
    }
    row.push(field);
    rows.push(row);
    rows.into_iter()
        .map(|row| row.into_iter().map(|f| f.trim().to_owned()).collect())
        .collect()
}
