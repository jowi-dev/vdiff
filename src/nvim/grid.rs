//! Pure Neovim `ext_linegrid` UI-protocol state machine: parsing raw
//! `redraw` notification batches into typed [`RedrawEvent`]s, and applying
//! them to a [`GridState`]. No I/O, no msgpack-rpc framing, no process
//! handling -- that's [`crate::nvim::session`]. This is the part of the
//! spike that's actually worth unit-testing thoroughly.
//!
//! Protocol reference: <https://neovim.io/doc/user/ui.html>. Only the
//! events a single-grid, no-multigrid-float client needs are handled;
//! everything else (`msg_showcmd`, `win_viewport`, popupmenu, etc.) is
//! silently ignored -- forward-compat with newer nvim versions that add
//! events this spike doesn't know about.

use std::collections::HashMap;

/// One cell in the grid: the glyph (nvim sends UTF-8 text, not a single
/// `char`, since some glyphs are multi-codepoint) and the highlight id to
/// look up in [`GridState::hl_attrs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub ch: String,
    pub hl_id: u64,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: " ".to_string(),
            hl_id: 0,
        }
    }
}

/// A highlight group's visual attributes, as sent by `hl_attr_define`.
/// Colors are `None` when the group doesn't override the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HlAttr {
    pub fg: Option<u32>,
    pub bg: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub reverse: bool,
    pub underline: bool,
}

/// A single, pre-parsed `redraw` event. Batches of these arrive together in
/// one msgpack-rpc notification; [`parse_redraw_batch`] turns the raw
/// `rmpv::Value` array into a `Vec` of these, and [`GridState::apply`]
/// applies them one at a time in order.
#[derive(Debug, Clone, PartialEq)]
pub enum RedrawEvent {
    GridResize {
        cols: usize,
        rows: usize,
    },
    GridClear,
    /// `cells` is already flattened: hl-id carry-over and `repeat` expansion
    /// happened in [`parse_redraw_batch`], so [`GridState::apply`] just
    /// writes them out starting at `col_start`.
    GridLine {
        row: usize,
        col_start: usize,
        cells: Vec<Cell>,
    },
    GridScroll {
        top: usize,
        bot: usize,
        left: usize,
        right: usize,
        rows: i64,
    },
    GridCursorGoto {
        row: usize,
        col: usize,
    },
    DefaultColorsSet {
        fg: u32,
        bg: u32,
    },
    HlAttrDefine {
        id: u64,
        attr: HlAttr,
    },
    Flush,
    /// An event this spike doesn't need (or a newer protocol addition it
    /// doesn't know about yet) -- carried through as a variant rather than
    /// dropped during parsing so callers/tests can see parsing didn't fail,
    /// it just has nothing to do.
    Unknown,
}

/// The full state of a single nvim grid: cell contents, cursor position,
/// and the highlight table needed to render them. `dirty` is set by any
/// event that changes what's on screen and cleared by [`Self::take_dirty`]
/// -- the renderer's signal that a repaint is worth doing (set for real by
/// [`RedrawEvent::Flush`], which is nvim's "the batch is visually
/// consistent now" marker).
#[derive(Debug, Clone, PartialEq)]
pub struct GridState {
    pub cols: usize,
    pub rows: usize,
    pub cells: Vec<Cell>,
    pub cursor: (usize, usize),
    pub hl_attrs: HashMap<u64, HlAttr>,
    pub default_fg: u32,
    pub default_bg: u32,
    dirty: bool,
}

impl GridState {
    /// A blank grid of `cols` x `rows` cells, defaults for everything else.
    pub fn new(cols: usize, rows: usize) -> Self {
        GridState {
            cols,
            rows,
            cells: vec![Cell::default(); cols * rows],
            cursor: (0, 0),
            hl_attrs: HashMap::new(),
            default_fg: 0xff_ff_ff,
            default_bg: 0x00_00_00,
            dirty: true,
        }
    }

    /// The cell at `(row, col)`, or `None` if out of bounds.
    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        if row >= self.rows || col >= self.cols {
            return None;
        }
        self.cells.get(row * self.cols + col)
    }

    /// Whether a flush has happened since the last [`Self::take_dirty`]
    /// call. Consumes the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Apply one parsed event, mutating `self` in place.
    pub fn apply(&mut self, event: &RedrawEvent) {
        match event {
            RedrawEvent::GridResize { cols, rows } => self.resize(*cols, *rows),
            RedrawEvent::GridClear => self.clear(),
            RedrawEvent::GridLine {
                row,
                col_start,
                cells,
            } => self.write_line(*row, *col_start, cells),
            RedrawEvent::GridScroll {
                top,
                bot,
                left,
                right,
                rows,
            } => self.scroll(*top, *bot, *left, *right, *rows),
            RedrawEvent::GridCursorGoto { row, col } => {
                self.cursor = (*row, *col);
                self.dirty = true;
            }
            RedrawEvent::DefaultColorsSet { fg, bg } => {
                self.default_fg = *fg;
                self.default_bg = *bg;
                self.dirty = true;
            }
            RedrawEvent::HlAttrDefine { id, attr } => {
                self.hl_attrs.insert(*id, *attr);
            }
            RedrawEvent::Flush => self.dirty = true,
            RedrawEvent::Unknown => {}
        }
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        self.cols = cols;
        self.rows = rows;
        self.cells = vec![Cell::default(); cols * rows];
        self.dirty = true;
    }

    fn clear(&mut self) {
        self.cells = vec![Cell::default(); self.cols * self.rows];
        self.dirty = true;
    }

    fn write_line(&mut self, row: usize, col_start: usize, cells: &[Cell]) {
        if row >= self.rows {
            return;
        }
        for (i, cell) in cells.iter().enumerate() {
            let col = col_start + i;
            if col >= self.cols {
                break;
            }
            self.cells[row * self.cols + col] = cell.clone();
        }
        self.dirty = true;
    }

    /// `rows > 0` scrolls content up (copies the rows below into the rows
    /// above, revealing new blank rows at the bottom of the region);
    /// `rows < 0` scrolls down. `cols` scrolling is not implemented -- nvim
    /// only ever sends 0 for it (see the module doc's protocol link).
    fn scroll(&mut self, top: usize, bot: usize, left: usize, right: usize, rows: i64) {
        if rows == 0 {
            return;
        }
        let width = right.saturating_sub(left);
        if width == 0 || bot <= top {
            return;
        }
        if rows > 0 {
            let rows = rows as usize;
            let mut y = top;
            while y + rows < bot {
                self.copy_row_range(y + rows, y, left, width);
                y += 1;
            }
            for y in y..bot {
                self.blank_row_range(y, left, width);
            }
        } else {
            let rows = (-rows) as usize;
            let mut y = bot;
            while y > top + rows {
                y -= 1;
                self.copy_row_range(y - rows, y, left, width);
            }
            for y in top..y {
                self.blank_row_range(y, left, width);
            }
        }
        self.dirty = true;
    }

    fn copy_row_range(&mut self, from_row: usize, to_row: usize, left: usize, width: usize) {
        for x in 0..width {
            let from = from_row * self.cols + left + x;
            let to = to_row * self.cols + left + x;
            self.cells[to] = self.cells[from].clone();
        }
    }

    fn blank_row_range(&mut self, row: usize, left: usize, width: usize) {
        for x in 0..width {
            self.cells[row * self.cols + left + x] = Cell::default();
        }
    }
}

/// Parse the `params` of a `redraw` notification -- an array of batches,
/// each `[event_name, args...]` where `args` are one or more parameter
/// tuples for that event (nvim coalesces repeated calls of the same event
/// within a batch) -- into a flat list of [`RedrawEvent`]s, in order.
///
/// `grid_line`'s per-cell `[text, hl_id?, repeat?]` encoding is expanded
/// here: a missing `hl_id` carries over from the previous cell in the same
/// `grid_line` call (starting from `0` if the very first cell omits it),
/// and `repeat` (default `1`) repeats that cell `repeat` times.
pub fn parse_redraw_batch(params: &[rmpv::Value]) -> Vec<RedrawEvent> {
    let mut events = Vec::new();
    for batch in params {
        let Some(items) = batch.as_array() else {
            continue;
        };
        let Some(name) = items.first().and_then(|v| v.as_str()) else {
            continue;
        };
        for args in &items[1..] {
            events.push(parse_one(name, args));
        }
    }
    events
}

fn parse_one(name: &str, args: &rmpv::Value) -> RedrawEvent {
    let empty = Vec::new();
    let a = args.as_array().unwrap_or(&empty);
    match name {
        "grid_resize" => RedrawEvent::GridResize {
            cols: u(a, 1),
            rows: u(a, 2),
        },
        "grid_clear" => RedrawEvent::GridClear,
        "grid_line" => parse_grid_line(a),
        "grid_scroll" => RedrawEvent::GridScroll {
            top: u(a, 1),
            bot: u(a, 2),
            left: u(a, 3),
            right: u(a, 4),
            rows: i(a, 5),
        },
        "grid_cursor_goto" => RedrawEvent::GridCursorGoto {
            row: u(a, 1),
            col: u(a, 2),
        },
        "default_colors_set" => RedrawEvent::DefaultColorsSet {
            fg: u32::try_from(i(a, 0)).unwrap_or(0xff_ff_ff),
            bg: u32::try_from(i(a, 1)).unwrap_or(0),
        },
        "hl_attr_define" => parse_hl_attr_define(a),
        "flush" => RedrawEvent::Flush,
        _ => RedrawEvent::Unknown,
    }
}

fn parse_grid_line(a: &[rmpv::Value]) -> RedrawEvent {
    let row = u(a, 1);
    let col_start = u(a, 2);
    let mut cells = Vec::new();
    let mut last_hl_id: u64 = 0;
    if let Some(raw_cells) = a.get(3).and_then(|v| v.as_array()) {
        for entry in raw_cells {
            let Some(fields) = entry.as_array() else {
                continue;
            };
            let text = fields
                .first()
                .and_then(|v| v.as_str())
                .unwrap_or(" ")
                .to_string();
            let hl_id = fields.get(1).and_then(|v| v.as_u64()).unwrap_or(last_hl_id);
            last_hl_id = hl_id;
            let repeat = fields.get(2).and_then(|v| v.as_u64()).unwrap_or(1).max(1);
            for _ in 0..repeat {
                cells.push(Cell {
                    ch: text.clone(),
                    hl_id,
                });
            }
        }
    }
    RedrawEvent::GridLine {
        row,
        col_start,
        cells,
    }
}

fn parse_hl_attr_define(a: &[rmpv::Value]) -> RedrawEvent {
    let id = a.first().and_then(|v| v.as_u64()).unwrap_or(0);
    let mut attr = HlAttr::default();
    if let Some(map) = a.get(1).and_then(|v| v.as_map()) {
        for (k, v) in map {
            let Some(key) = k.as_str() else { continue };
            match key {
                "foreground" => attr.fg = v.as_u64().and_then(|n| u32::try_from(n).ok()),
                "background" => attr.bg = v.as_u64().and_then(|n| u32::try_from(n).ok()),
                "bold" => attr.bold = v.as_bool().unwrap_or(false),
                "italic" => attr.italic = v.as_bool().unwrap_or(false),
                "reverse" => attr.reverse = v.as_bool().unwrap_or(false),
                "underline" => attr.underline = v.as_bool().unwrap_or(false),
                _ => {}
            }
        }
    }
    RedrawEvent::HlAttrDefine { id, attr }
}

/// Fetch `a[idx]` as a `usize`, defaulting to `0` if missing or not an
/// integer -- every positional field these events use is a non-negative
/// grid coordinate or count.
fn u(a: &[rmpv::Value], idx: usize) -> usize {
    a.get(idx).and_then(|v| v.as_u64()).unwrap_or(0) as usize
}

/// Fetch `a[idx]` as an `i64`, defaulting to `0` -- used for `grid_scroll`'s
/// signed `rows`/`cols` fields.
fn i(a: &[rmpv::Value], idx: usize) -> i64 {
    a.get(idx).and_then(|v| v.as_i64()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmpv::Value;

    fn arr(items: Vec<Value>) -> Value {
        Value::Array(items)
    }

    fn s(text: &str) -> Value {
        Value::String(text.into())
    }

    #[test]
    fn grid_resize_reallocates_cells() {
        let mut grid = GridState::new(2, 2);
        let batch = vec![arr(vec![
            s("grid_resize"),
            arr(vec![1.into(), 10.into(), 5.into()]),
        ])];
        let events = parse_redraw_batch(&batch);
        assert_eq!(events, vec![RedrawEvent::GridResize { cols: 10, rows: 5 }]);
        grid.apply(&events[0]);
        assert_eq!(grid.cols, 10);
        assert_eq!(grid.rows, 5);
        assert_eq!(grid.cells.len(), 50);
    }

    #[test]
    fn grid_clear_blanks_every_cell() {
        let mut grid = GridState::new(2, 1);
        grid.cells[0] = Cell {
            ch: "x".into(),
            hl_id: 3,
        };
        grid.apply(&RedrawEvent::GridClear);
        assert_eq!(grid.cells, vec![Cell::default(), Cell::default()]);
    }

    #[test]
    fn grid_line_hl_id_carries_over_when_omitted() {
        // cells: ["a", 5], ["b"] (no hl_id -> carries 5), ["c", 7]
        let cells_value = arr(vec![
            arr(vec![s("a"), 5.into()]),
            arr(vec![s("b")]),
            arr(vec![s("c"), 7.into()]),
        ]);
        let batch = vec![arr(vec![
            s("grid_line"),
            arr(vec![1.into(), 0.into(), 0.into(), cells_value]),
        ])];
        let events = parse_redraw_batch(&batch);
        let RedrawEvent::GridLine { cells, .. } = &events[0] else {
            panic!("expected GridLine");
        };
        assert_eq!(
            cells,
            &vec![
                Cell {
                    ch: "a".into(),
                    hl_id: 5
                },
                Cell {
                    ch: "b".into(),
                    hl_id: 5
                },
                Cell {
                    ch: "c".into(),
                    hl_id: 7
                },
            ]
        );
    }

    #[test]
    fn grid_line_repeat_expands_cells() {
        // [" ", 0, 4] -> four blank cells at hl 0.
        let cells_value = arr(vec![arr(vec![s(" "), 0.into(), 4.into()])]);
        let batch = vec![arr(vec![
            s("grid_line"),
            arr(vec![0.into(), 0.into(), 2.into(), cells_value]),
        ])];
        let events = parse_redraw_batch(&batch);
        let RedrawEvent::GridLine {
            row,
            col_start,
            cells,
        } = &events[0]
        else {
            panic!("expected GridLine");
        };
        assert_eq!(*row, 0);
        assert_eq!(*col_start, 2);
        assert_eq!(cells.len(), 4);
        assert!(cells.iter().all(|c| c.ch == " " && c.hl_id == 0));
    }

    #[test]
    fn grid_line_applies_starting_at_col_start() {
        let mut grid = GridState::new(5, 1);
        let event = RedrawEvent::GridLine {
            row: 0,
            col_start: 2,
            cells: vec![
                Cell {
                    ch: "x".into(),
                    hl_id: 1,
                },
                Cell {
                    ch: "y".into(),
                    hl_id: 1,
                },
            ],
        };
        grid.apply(&event);
        assert_eq!(grid.cell(0, 0), Some(&Cell::default()));
        assert_eq!(
            grid.cell(0, 2),
            Some(&Cell {
                ch: "x".into(),
                hl_id: 1
            })
        );
        assert_eq!(
            grid.cell(0, 3),
            Some(&Cell {
                ch: "y".into(),
                hl_id: 1
            })
        );
    }

    #[test]
    fn grid_scroll_positive_rows_moves_content_up() {
        // 4 rows, 1 col. Fill row i with a distinct char, scroll region
        // [0,4) by rows=2: expect row0<-row2, row1<-row3, rows 2,3 blanked.
        let mut grid = GridState::new(1, 4);
        for row in 0..4 {
            grid.cells[row] = Cell {
                ch: ((b'a' + row as u8) as char).to_string(),
                hl_id: 0,
            };
        }
        grid.apply(&RedrawEvent::GridScroll {
            top: 0,
            bot: 4,
            left: 0,
            right: 1,
            rows: 2,
        });
        assert_eq!(grid.cell(0, 0).unwrap().ch, "c");
        assert_eq!(grid.cell(1, 0).unwrap().ch, "d");
        assert_eq!(grid.cell(2, 0).unwrap().ch, " ");
        assert_eq!(grid.cell(3, 0).unwrap().ch, " ");
    }

    #[test]
    fn grid_scroll_negative_rows_moves_content_down() {
        let mut grid = GridState::new(1, 4);
        for row in 0..4 {
            grid.cells[row] = Cell {
                ch: ((b'a' + row as u8) as char).to_string(),
                hl_id: 0,
            };
        }
        grid.apply(&RedrawEvent::GridScroll {
            top: 0,
            bot: 4,
            left: 0,
            right: 1,
            rows: -2,
        });
        assert_eq!(grid.cell(0, 0).unwrap().ch, " ");
        assert_eq!(grid.cell(1, 0).unwrap().ch, " ");
        assert_eq!(grid.cell(2, 0).unwrap().ch, "a");
        assert_eq!(grid.cell(3, 0).unwrap().ch, "b");
    }

    #[test]
    fn grid_cursor_goto_updates_cursor() {
        let mut grid = GridState::new(10, 10);
        grid.apply(&RedrawEvent::GridCursorGoto { row: 3, col: 4 });
        assert_eq!(grid.cursor, (3, 4));
    }

    #[test]
    fn default_colors_set_updates_defaults() {
        let mut grid = GridState::new(1, 1);
        grid.apply(&RedrawEvent::DefaultColorsSet {
            fg: 0x11_22_33,
            bg: 0x44_55_66,
        });
        assert_eq!(grid.default_fg, 0x11_22_33);
        assert_eq!(grid.default_bg, 0x44_55_66);
    }

    #[test]
    fn hl_attr_define_parses_map_fields() {
        let map = Value::Map(vec![
            (s("foreground"), Value::from(0x00_ff_00u32)),
            (s("background"), Value::from(0x00_00_ffu32)),
            (s("bold"), Value::from(true)),
            (s("italic"), Value::from(false)),
            (s("reverse"), Value::from(true)),
            (s("underline"), Value::from(false)),
        ]);
        let batch = vec![arr(vec![
            s("hl_attr_define"),
            arr(vec![9.into(), map, arr(vec![]), arr(vec![])]),
        ])];
        let events = parse_redraw_batch(&batch);
        assert_eq!(
            events,
            vec![RedrawEvent::HlAttrDefine {
                id: 9,
                attr: HlAttr {
                    fg: Some(0x00_ff_00),
                    bg: Some(0x00_00_ff),
                    bold: true,
                    italic: false,
                    reverse: true,
                    underline: false,
                },
            }]
        );
        let mut grid = GridState::new(1, 1);
        grid.apply(&events[0]);
        assert_eq!(grid.hl_attrs.get(&9), Some(&events_attr(&events)));
    }

    fn events_attr(events: &[RedrawEvent]) -> HlAttr {
        match &events[0] {
            RedrawEvent::HlAttrDefine { attr, .. } => *attr,
            _ => panic!("expected HlAttrDefine"),
        }
    }

    #[test]
    fn flush_marks_dirty() {
        let mut grid = GridState::new(1, 1);
        grid.take_dirty(); // clear the initial dirty-on-construction flag
        assert!(!grid.take_dirty(), "already consumed");
        grid.apply(&RedrawEvent::Flush);
        assert!(grid.take_dirty());
        assert!(!grid.take_dirty(), "consumed again");
    }

    #[test]
    fn unknown_event_name_parses_to_unknown_variant() {
        let batch = vec![arr(vec![s("win_viewport"), arr(vec![])])];
        let events = parse_redraw_batch(&batch);
        assert_eq!(events, vec![RedrawEvent::Unknown]);
    }

    #[test]
    fn multiple_calls_in_one_batch_item_all_parsed() {
        // grid_line can appear multiple times per batch entry: ["grid_line",
        // args1, args2].
        let cells_value = arr(vec![arr(vec![s("x")])]);
        let batch = vec![arr(vec![
            s("grid_line"),
            arr(vec![0.into(), 0.into(), 0.into(), cells_value.clone()]),
            arr(vec![0.into(), 1.into(), 0.into(), cells_value]),
        ])];
        let events = parse_redraw_batch(&batch);
        assert_eq!(events.len(), 2);
    }
}
