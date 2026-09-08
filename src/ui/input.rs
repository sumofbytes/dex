use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// A small UTF-8-aware multiline editor used by the terminal UI.
pub(crate) struct InputField {
    pub(super) lines: Vec<String>,
    pub(super) row: usize,
    pub(super) col: usize,
}

impl InputField {
    pub(super) fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    pub(super) fn from_text(text: &str) -> Self {
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n')
                .map(|s| {
                    s.chars()
                        .filter(|c| *c == '\t' || !c.is_control())
                        .collect()
                })
                .collect()
        };
        let row = lines.len().saturating_sub(1);
        let col = lines[row].len();
        Self { lines, row, col }
    }

    pub(super) fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub(super) fn reset(&mut self) {
        self.lines = vec![String::new()];
        self.row = 0;
        self.col = 0;
    }

    pub(super) fn insert_char(&mut self, c: char) {
        if c == '\n' {
            self.clamp_col();
            let line = std::mem::take(&mut self.lines[self.row]);
            let (left, right) = line.split_at(self.col);
            self.lines.insert(self.row + 1, right.to_string());
            self.lines[self.row] = left.to_string();
            self.row += 1;
            self.col = 0;
            return;
        }
        self.clamp_col();
        self.lines[self.row].insert(self.col, c);
        self.col += c.len_utf8();
    }

    /// Insert a whole pasted chunk: tabs expand to four spaces (they would
    /// render as tab stops and desync the frame), CR / LF / CRLF each become
    /// one composer newline, and other control characters are dropped.
    pub(super) fn insert_paste(&mut self, text: &str) {
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    self.insert_char('\n');
                }
                '\n' => self.insert_char('\n'),
                '\t' => {
                    for _ in 0..4 {
                        self.insert_char(' ');
                    }
                }
                c if !c.is_control() => self.insert_char(c),
                _ => {}
            }
        }
    }

    pub(super) fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                if !c.is_control() {
                    self.insert_char(c);
                }
            }
            KeyCode::Char('j')
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                // Ctrl+J (LF, 0x0A) is the universal newline fallback:
                // legacy terminals report Shift+Enter as bare `\r`, identical
                // to Enter, until the Kitty disambiguate flag is pushed (remote.rs).
                // Ctrl+J arrives as a distinct byte everywhere — same
                // convention as Codex/opencode — so it always means newline.
                // SHIFT is tolerated (Ctrl+Shift+J still newlines); ALT is
                // excluded so Alt-chorded bindings stay reserved.
                self.insert_char('\n');
            }
            KeyCode::Char(_) => {}
            KeyCode::Enter => self.insert_char('\n'),
            KeyCode::Backspace => {
                if self.col == 0 {
                    if self.row > 0 {
                        let removed = self.lines.remove(self.row);
                        self.row -= 1;
                        self.col = self.lines[self.row].len();
                        self.lines[self.row].push_str(&removed);
                    }
                } else {
                    let line = &mut self.lines[self.row];
                    let mut idx = self.col;
                    while idx > 0 && !line.is_char_boundary(idx - 1) {
                        idx -= 1;
                    }
                    line.remove(idx - 1);
                    self.col = idx - 1;
                }
            }
            KeyCode::Delete => {
                let line = &mut self.lines[self.row];
                if self.col < line.len() {
                    let mut idx = self.col;
                    while idx < line.len() && !line.is_char_boundary(idx + 1) {
                        idx += 1;
                    }
                    line.remove(idx);
                } else if self.row + 1 < self.lines.len() {
                    let removed = self.lines.remove(self.row + 1);
                    self.lines[self.row].push_str(&removed);
                }
            }
            KeyCode::Left => {
                if self.col > 0 {
                    let line = &self.lines[self.row];
                    let mut idx = self.col;
                    while idx > 0 && !line.is_char_boundary(idx - 1) {
                        idx -= 1;
                    }
                    self.col = idx - 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].len();
                }
            }
            KeyCode::Right => {
                let line = &self.lines[self.row];
                if self.col < line.len() {
                    let mut idx = self.col;
                    while idx < line.len() && !line.is_char_boundary(idx + 1) {
                        idx += 1;
                    }
                    self.col = idx + 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            KeyCode::Up if self.row > 0 => {
                self.row -= 1;
                self.clamp_col();
            }
            KeyCode::Down if self.row + 1 < self.lines.len() => {
                self.row += 1;
                self.clamp_col();
            }
            KeyCode::Home => self.col = 0,
            KeyCode::End => self.col = self.lines[self.row].len(),
            KeyCode::Tab => self.insert_char('\t'),
            _ => {}
        }
    }

    /// Clamp `col` to a char boundary on the current row. A byte offset that
    /// is a boundary on one row can land inside a multi-byte char on another
    /// (arrow-key row moves through pasted multi-line text); `String::insert`
    /// / `split_at` panic on such an offset — which exits the whole TUI — so
    /// the cursor snaps left to the char start instead.
    fn clamp_col(&mut self) {
        let line = &self.lines[self.row];
        let mut col = self.col.min(line.len());
        while col > 0 && !line.is_char_boundary(col) {
            col -= 1;
        }
        self.col = col;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEventKind, KeyEventState};

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: mods,
            kind: KeyEventKind::Press,
            state: KeyEventState::empty(),
        }
    }

    #[test]
    fn alt_and_ctrl_modified_chars_are_dropped() {
        // Correct behavior: Alt/Ctrl-modified chars must never land in the
        // composer. `\x1b]10;rgb:…\x07` is parsed by crossterm as Alt+`]` +
        // plain `10;rgb:…` + Ctrl-G. The Alt/Ctrl wrappers are dropped here;
        // the plain middle is consumed at the source by the timed drain in
        // `remote.rs` before the event loop starts, so it never reaches
        // `handle_key`. This test verifies the input guard only.
        let mut f = InputField::new();
        f.handle_key(key(KeyCode::Char(']'), KeyModifiers::ALT));
        assert_eq!(f.text(), "", "Alt+] must be dropped");
        f.handle_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(f.text(), "", "Ctrl-G (BEL) must be dropped");
        f.handle_key(key(KeyCode::Char('\\'), KeyModifiers::ALT));
        assert_eq!(f.text(), "", "Alt+\\ (ST) must be dropped");
        // Plain burst would reach input only if drain failed — input itself
        // correctly inserts plain, drain is the source fix.
        for c in "10;rgb:f6f6/dcdc/acac".chars() {
            f.handle_key(key(KeyCode::Char(c), KeyModifiers::empty()));
        }
        assert!(
            f.text().contains("10;rgb:"),
            "plain is inserted when it reaches input"
        );
    }

    #[test]
    fn ctrl_j_inserts_newline_like_shift_enter() {
        // Ctrl+J (LF) is the universal newline fallback for terminals that
        // can't report Shift+Enter distinctly (no Kitty protocol): crossterm
        // delivers it as Char('j') + CONTROL in raw mode, never as Enter.
        let mut f = InputField::from_text("ab");
        f.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(f.text(), "ab\n", "Ctrl+J must insert a newline");
        // Ctrl+Shift+J still newlines (SHIFT tolerated).
        f.handle_key(key(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(f.text(), "ab\n\n", "Ctrl+Shift+J must insert a newline");
        // Alt-chorded Ctrl+J stays reserved (dropped).
        f.handle_key(key(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        ));
        assert_eq!(f.text(), "ab\n\n", "Ctrl+Alt+J must stay dropped");
        // Other Ctrl-modified chars are still dropped (OSC-report guard).
        f.handle_key(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert_eq!(f.text(), "ab\n\n", "Ctrl+G must stay dropped");
    }

    #[test]
    fn row_move_into_multibyte_line_snaps_cursor() {
        // Regression: clamp_col clamped to byte length only, so a byte offset
        // that was a boundary on the previous row landed inside a multi-byte
        // char after Up/Down; the next keystroke panicked `String::insert`
        // and exited the TUI. Paste multi-line text with a wide char, arrow
        // across rows, then type.
        let mut f = InputField::from_text("abcd\nab\u{7AC7}def"); // 界 = bytes 2..5
        assert_eq!(f.row, 1);
        f.handle_key(key(KeyCode::Up, KeyModifiers::empty()));
        assert_eq!(f.row, 0);
        assert_eq!(f.col, 4);
        f.handle_key(key(KeyCode::Down, KeyModifiers::empty()));
        // Byte 4 sits inside 界 on this row: cursor snaps to the char start.
        assert_eq!(f.row, 1);
        assert_eq!(f.col, 2);
        f.handle_key(key(KeyCode::Char('i'), KeyModifiers::empty()));
        assert_eq!(f.lines[1], "abi\u{7AC7}def");
        // Enter at a snapped col must not panic either.
        f.handle_key(key(KeyCode::Enter, KeyModifiers::empty()));
        assert_eq!(f.lines[1], "abi");
        assert_eq!(f.lines[2], "\u{7AC7}def");
    }

    #[test]
    fn paste_becomes_multiline_and_expands_tabs() {
        let mut f = InputField::new();
        // CRLF, CR and LF each count as exactly one newline; a tab widens to
        // four spaces so the frame never desyncs on tab stops.
        f.insert_paste("a\r\nb\rc\nd\t!");
        assert_eq!(
            f.lines,
            vec![
                "a".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d    !".to_string()
            ]
        );
        // Cursor rests at the end of the pasted text.
        assert_eq!(f.row, 3);
        assert_eq!(f.col, f.lines[3].len());
        // Other control characters (ESC etc.) are dropped, not typed.
        let mut g = InputField::new();
        g.insert_paste("\x1b[31mred");
        assert_eq!(g.text(), "[31mred");
        // Empty paste is a no-op.
        let mut h = InputField::new();
        h.insert_paste("");
        assert_eq!(h.lines, vec![String::new()]);
    }

    #[test]
    fn normal_typing_still_works() {
        let mut f = InputField::new();
        for c in "hello".chars() {
            f.handle_key(key(KeyCode::Char(c), KeyModifiers::empty()));
        }
        assert_eq!(f.text(), "hello");
        // Shift+letter (uppercase) must still insert.
        f.handle_key(key(KeyCode::Char('W'), KeyModifiers::SHIFT));
        assert_eq!(f.text(), "helloW");
        // Ctrl+C must not insert.
        f.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(f.text(), "helloW");
    }
}
