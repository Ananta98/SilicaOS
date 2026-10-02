// SPDX-License-Identifier: GPL-2.0

//! Virtual Terminal Framebuffer Console (`fbcon` / `vt_term`).
//!
//! Provides a hardware-accelerated text console on top of the linear framebuffer,
//! supporting a full character cell grid, hardware cursor management, automatic
//! line wrapping, smooth scrolling, and ANSI escape sequence rendering.

use alloc::{sync::Arc, vec, vec::Vec};

use super::{
    ansi::{AnsiAction, AnsiParser},
    fb::Framebuffer,
    font::{FONT_HEIGHT, FONT_WIDTH, get_glyph},
    pixel::Color,
};

/// A single cell in the console's character grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScreenCell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub inverse: bool,
}

impl ScreenCell {
    pub const fn blank(fg: Color, bg: Color) -> Self {
        Self {
            ch: ' ',
            fg,
            bg,
            bold: false,
            inverse: false,
        }
    }
}

/// A Linux `fbcon` / FreeBSD `vt` style virtual terminal console.
pub struct FbConsole {
    fb: Arc<Framebuffer>,
    pub cols: usize,
    pub rows: usize,
    pub col: usize,
    pub row: usize,
    pub cursor_visible: bool,
    cursor_drawn: bool,
    default_fg: Color,
    default_bg: Color,
    current_fg: Color,
    current_bg: Color,
    bold: bool,
    inverse: bool,
    cells: Vec<ScreenCell>,
    ansi_parser: AnsiParser,
}

impl FbConsole {
    /// Creates and initializes a new virtual terminal console on the framebuffer.
    pub fn new(fb: Arc<Framebuffer>) -> Self {
        let cols = fb.width / FONT_WIDTH;
        let rows = fb.height / FONT_HEIGHT;
        let default_fg = Color::WHITE;
        let default_bg = Color::BLACK;

        let total_cells = cols * rows;
        let cells = vec![ScreenCell::blank(default_fg, default_bg); total_cells];

        // Clear the physical surface
        fb.clear(default_bg);

        let mut console = Self {
            fb,
            cols,
            rows,
            col: 0,
            row: 0,
            cursor_visible: true,
            cursor_drawn: false,
            default_fg,
            default_bg,
            current_fg: default_fg,
            current_bg: default_bg,
            bold: false,
            inverse: false,
            cells,
            ansi_parser: AnsiParser::new(),
        };

        console.draw_cursor();
        console
    }

    /// Redraws a character cell at grid position `(col, row)`.
    pub fn draw_cell(&self, col: usize, row: usize) {
        if col >= self.cols || row >= self.rows {
            return;
        }

        let cell = self.cells[row * self.cols + col];
        let (mut fg, bg) = if cell.inverse {
            (cell.bg, cell.fg)
        } else {
            (cell.fg, cell.bg)
        };

        if cell.bold {
            // Brighten standard foreground color if bold
            if fg == Color::WHITE {
                fg = Color::BRIGHT_WHITE;
            }
        }

        let glyph = get_glyph(cell.ch as u8);
        self.fb.draw_glyph(col * FONT_WIDTH, row * FONT_HEIGHT, glyph, fg, bg);
    }

    /// Renders the hardware text cursor at current `(self.col, self.row)`.
    pub fn draw_cursor(&mut self) {
        if !self.cursor_visible || self.cursor_drawn || self.col >= self.cols || self.row >= self.rows {
            return;
        }

        let x = self.col * FONT_WIDTH;
        let y = self.row * FONT_HEIGHT;

        // Draw a solid 2-pixel underline cursor at the bottom of the current cell
        let cursor_height = 2;
        let cursor_y = y + FONT_HEIGHT.saturating_sub(cursor_height);
        self.fb.fill_rect(x, cursor_y, FONT_WIDTH, cursor_height, Color::BRIGHT_WHITE);

        self.cursor_drawn = true;
    }

    /// Erases the cursor by redrawing the underlying character cell.
    pub fn erase_cursor(&mut self) {
        if !self.cursor_drawn {
            return;
        }

        self.draw_cell(self.col, self.row);
        self.cursor_drawn = false;
    }

    /// Appends a printable character at the cursor position.
    pub fn put_char(&mut self, ch: char) {
        if self.col >= self.cols {
            self.newline();
        }

        let idx = self.row * self.cols + self.col;
        if idx < self.cells.len() {
            self.cells[idx] = ScreenCell {
                ch,
                fg: self.current_fg,
                bg: self.current_bg,
                bold: self.bold,
                inverse: self.inverse,
            };
            self.draw_cell(self.col, self.row);
        }

        self.col += 1;
    }

    /// Advances the cursor to a new line, scrolling if at the bottom row.
    pub fn newline(&mut self) {
        self.col = 0;
        if self.row + 1 < self.rows {
            self.row += 1;
        } else {
            self.scroll_up(1);
        }
    }

    /// Scrolls the console buffer and physical display upward by `count` lines.
    pub fn scroll_up(&mut self, count: usize) {
        if count == 0 {
            return;
        }

        let lines = count.min(self.rows);
        self.fb.scroll_up(lines * FONT_HEIGHT, self.default_bg);

        // Shift cell rows in memory
        let shift_cells = lines * self.cols;
        let total_cells = self.cols * self.rows;

        for i in 0..(total_cells.saturating_sub(shift_cells)) {
            self.cells[i] = self.cells[i + shift_cells];
        }

        // Fill newly freed bottom lines with blank cells
        let blank = ScreenCell::blank(self.default_fg, self.default_bg);
        for i in (total_cells.saturating_sub(shift_cells))..total_cells {
            self.cells[i] = blank;
        }
    }

    /// Moves cursor to start of current row (Carriage Return).
    pub fn carriage_return(&mut self) {
        self.col = 0;
    }

    /// Advances to the next 8-column tab stop.
    pub fn tab(&mut self) {
        let next_tab = ((self.col / 8) + 1) * 8;
        if next_tab >= self.cols {
            self.newline();
        } else {
            self.col = next_tab;
        }
    }

    /// Destructive backspace.
    pub fn backspace(&mut self) {
        if self.col > 0 {
            self.col -= 1;
            let idx = self.row * self.cols + self.col;
            if idx < self.cells.len() {
                self.cells[idx] = ScreenCell::blank(self.default_fg, self.default_bg);
                self.draw_cell(self.col, self.row);
            }
        }
    }

    /// Clears the display according to mode (0: cursor to end, 1: start to cursor, 2: full display).
    pub fn clear_display(&mut self, mode: u8) {
        match mode {
            2 => {
                // Entire screen
                self.fb.clear(self.default_bg);
                let blank = ScreenCell::blank(self.default_fg, self.default_bg);
                for cell in self.cells.iter_mut() {
                    *cell = blank;
                }
                self.col = 0;
                self.row = 0;
            }
            0 => {
                // From cursor to end of screen
                let start_idx = self.row * self.cols + self.col;
                let blank = ScreenCell::blank(self.default_fg, self.default_bg);
                for i in start_idx..self.cells.len() {
                    self.cells[i] = blank;
                }
                // Clear the rest of current row
                let rest_w = (self.cols - self.col) * FONT_WIDTH;
                self.fb.fill_rect(self.col * FONT_WIDTH, self.row * FONT_HEIGHT, rest_w, FONT_HEIGHT, self.default_bg);
                // Clear remaining rows below
                if self.row + 1 < self.rows {
                    let y = (self.row + 1) * FONT_HEIGHT;
                    let h = (self.rows - (self.row + 1)) * FONT_HEIGHT;
                    self.fb.fill_rect(0, y, self.fb.width, h, self.default_bg);
                }
            }
            1 => {
                // From start of screen to cursor
                let end_idx = (self.row * self.cols + self.col).min(self.cells.len());
                let blank = ScreenCell::blank(self.default_fg, self.default_bg);
                for i in 0..=end_idx {
                    if i < self.cells.len() {
                        self.cells[i] = blank;
                    }
                }
                // Clear preceding rows
                if self.row > 0 {
                    self.fb.fill_rect(0, 0, self.fb.width, self.row * FONT_HEIGHT, self.default_bg);
                }
                // Clear start of current row up to cursor
                self.fb.fill_rect(0, self.row * FONT_HEIGHT, (self.col + 1) * FONT_WIDTH, FONT_HEIGHT, self.default_bg);
            }
            _ => {}
        }
    }

    /// Clears the line according to mode (0: cursor to end, 1: start to cursor, 2: full line).
    pub fn clear_line(&mut self, mode: u8) {
        if self.row >= self.rows {
            return;
        }

        let blank = ScreenCell::blank(self.default_fg, self.default_bg);
        let row_start = self.row * self.cols;

        match mode {
            2 => {
                // Whole line
                for c in 0..self.cols {
                    self.cells[row_start + c] = blank;
                }
                self.fb.fill_rect(0, self.row * FONT_HEIGHT, self.fb.width, FONT_HEIGHT, self.default_bg);
            }
            0 => {
                // Cursor to end of line
                for c in self.col..self.cols {
                    self.cells[row_start + c] = blank;
                }
                let w = (self.cols.saturating_sub(self.col)) * FONT_WIDTH;
                self.fb.fill_rect(self.col * FONT_WIDTH, self.row * FONT_HEIGHT, w, FONT_HEIGHT, self.default_bg);
            }
            1 => {
                // Start of line to cursor
                for c in 0..=self.col.min(self.cols.saturating_sub(1)) {
                    self.cells[row_start + c] = blank;
                }
                let w = (self.col + 1).min(self.cols) * FONT_WIDTH;
                self.fb.fill_rect(0, self.row * FONT_HEIGHT, w, FONT_HEIGHT, self.default_bg);
            }
            _ => {}
        }
    }

    /// Executes a parsed ANSI action on the console.
    fn execute_action(&mut self, action: AnsiAction) {
        match action {
            AnsiAction::Print(ch) => self.put_char(ch),
            AnsiAction::Newline => self.newline(),
            AnsiAction::CarriageReturn => self.carriage_return(),
            AnsiAction::Tab => self.tab(),
            AnsiAction::Backspace => self.backspace(),
            AnsiAction::SetStyle { fg, bg, bold, inverse, reset } => {
                if reset {
                    self.current_fg = self.default_fg;
                    self.current_bg = self.default_bg;
                    self.bold = false;
                    self.inverse = false;
                }
                if let Some(c) = fg {
                    self.current_fg = c;
                }
                if let Some(c) = bg {
                    self.current_bg = c;
                }
                if bold {
                    self.bold = true;
                }
                if inverse {
                    self.inverse = true;
                }
            }
            AnsiAction::SetCursorPos { col, row } => {
                self.col = col.min(self.cols.saturating_sub(1));
                self.row = row.min(self.rows.saturating_sub(1));
            }
            AnsiAction::MoveCursor { dcol, drow } => {
                let new_col = (self.col as isize + dcol).max(0).min(self.cols as isize - 1);
                let new_row = (self.row as isize + drow).max(0).min(self.rows as isize - 1);
                self.col = new_col as usize;
                self.row = new_row as usize;
            }
            AnsiAction::ClearDisplay(mode) => self.clear_display(mode),
            AnsiAction::ClearLine(mode) => self.clear_line(mode),
            AnsiAction::SetCursorVisible(visible) => {
                self.cursor_visible = visible;
            }
            AnsiAction::Reset => {
                self.current_fg = self.default_fg;
                self.current_bg = self.default_bg;
                self.bold = false;
                self.inverse = false;
                self.cursor_visible = true;
                self.clear_display(2);
            }
        }
    }

    /// Writes a single byte into the terminal, evaluating escape codes.
    pub fn write_byte(&mut self, byte: u8) {
        self.erase_cursor();
        let mut parser = core::mem::replace(&mut self.ansi_parser, AnsiParser::new());
        parser.process_byte(byte, |action| {
            self.execute_action(action);
        });
        self.ansi_parser = parser;
        self.draw_cursor();
    }

    /// Writes a string slice into the terminal.
    pub fn write_str(&mut self, s: &str) {
        self.erase_cursor();
        let mut parser = core::mem::replace(&mut self.ansi_parser, AnsiParser::new());
        for &b in s.as_bytes() {
            parser.process_byte(b, |action| {
                self.execute_action(action);
            });
        }
        self.ansi_parser = parser;
        self.draw_cursor();
    }

    /// Writes raw byte buffer into the terminal.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        self.erase_cursor();
        let mut parser = core::mem::replace(&mut self.ansi_parser, AnsiParser::new());
        for &b in bytes {
            parser.process_byte(b, |action| {
                self.execute_action(action);
            });
        }
        self.ansi_parser = parser;
        self.draw_cursor();
    }
}
