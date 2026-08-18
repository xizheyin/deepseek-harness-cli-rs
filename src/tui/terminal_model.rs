//! A deliberately small terminal emulator used to verify the enhanced
//! renderer's screen and scrollback effects. Unknown control sequences fail
//! the test instead of being guessed.

use unicode_width::UnicodeWidthChar as _;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HistoryPolicy {
    FullScreenOnly,
    TopAnchoredRegion,
}

pub(crate) struct MiniTerminal {
    rows: usize,
    columns: usize,
    cells: Vec<Vec<String>>,
    history: Vec<String>,
    row: usize,
    column: usize,
    top: usize,
    bottom: usize,
    origin: bool,
    wrap_pending: bool,
    partial_margin_seen: bool,
    policy: HistoryPolicy,
}

impl MiniTerminal {
    pub(crate) fn prefilled(rows: usize, columns: usize, policy: HistoryPolicy) -> Self {
        let mut terminal = Self::blank(rows, columns, policy);
        for row in 0..rows {
            terminal.write_at(row, 0, &format!("PRE{:02}", row + 1));
        }
        terminal.row = rows - 1;
        terminal.column = format!("PRE{rows:02}").len();
        terminal
    }

    pub(crate) fn blank(rows: usize, columns: usize, policy: HistoryPolicy) -> Self {
        assert!(rows != 0 && columns != 0);
        Self {
            rows,
            columns,
            cells: vec![vec![String::new(); columns]; rows],
            history: Vec::new(),
            row: 0,
            column: 0,
            top: 0,
            bottom: rows - 1,
            origin: false,
            wrap_pending: false,
            partial_margin_seen: false,
            policy,
        }
    }

    pub(crate) fn feed(&mut self, input: &[u8]) {
        let text = std::str::from_utf8(input).expect("screen transactions must be UTF-8");
        let bytes = text.as_bytes();
        let mut index = 0_usize;
        while index < bytes.len() {
            match bytes[index] {
                0x1b => {
                    assert_eq!(bytes.get(index + 1), Some(&b'['), "only CSI is supported");
                    let start = index + 2;
                    let mut end = start;
                    while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                        end += 1;
                    }
                    assert!(end < bytes.len(), "unterminated CSI");
                    self.csi(&text[start..end], char::from(bytes[end]));
                    index = end + 1;
                }
                b'\r' => {
                    self.column = 0;
                    self.wrap_pending = false;
                    index += 1;
                }
                b'\n' => {
                    // The production terminal contract keeps OPOST+ONLCR, so
                    // an application LF arrives at the emulator as CRLF.
                    self.column = 0;
                    self.line_feed();
                    index += 1;
                }
                byte if byte < 0x20 => panic!("unexpected control byte {byte:#x}"),
                _ => {
                    let character = text[index..]
                        .chars()
                        .next()
                        .expect("the current byte starts a UTF-8 scalar");
                    self.print(character);
                    index += character.len_utf8();
                }
            }
        }
    }

    pub(crate) const fn partial_margin_seen(&self) -> bool {
        self.partial_margin_seen
    }

    pub(crate) fn history(&self) -> &[String] {
        &self.history
    }

    pub(crate) fn all_lines(&self) -> Vec<String> {
        self.history
            .iter()
            .cloned()
            .chain((0..self.rows).map(|row| self.line(row)))
            .collect()
    }

    pub(crate) fn grow_top_anchored(&mut self, rows: usize) {
        assert!(rows >= self.rows, "this model operation only grows height");
        self.cells
            .extend((self.rows..rows).map(|_| vec![String::new(); self.columns]));
        self.rows = rows;
        self.bottom = rows - 1;
    }

    fn csi(&mut self, parameters: &str, final_byte: char) {
        match final_byte {
            'H' | 'f' => {
                let (row, column) = pair(parameters, 1, 1);
                let base = if self.origin { self.top } else { 0 };
                self.row = (base + row.saturating_sub(1)).min(self.rows - 1);
                self.column = column.saturating_sub(1).min(self.columns - 1);
                self.wrap_pending = false;
            }
            'K' => {
                assert_eq!(number(parameters, 0), 2, "only EL2 is supported");
                self.cells[self.row].fill(String::new());
                self.wrap_pending = false;
            }
            'J' => {
                assert_eq!(number(parameters, 0), 2, "only ED2 is supported");
                for row in &mut self.cells {
                    row.fill(String::new());
                }
                self.wrap_pending = false;
            }
            'r' if parameters.is_empty() => {
                self.top = 0;
                self.bottom = self.rows - 1;
                self.row = 0;
                self.column = 0;
                self.wrap_pending = false;
            }
            'r' => {
                let (top, bottom) = pair(parameters, 1, self.rows);
                self.top = top.saturating_sub(1).min(self.rows - 1);
                self.bottom = bottom.saturating_sub(1).min(self.rows - 1);
                self.partial_margin_seen |= self.top != 0 || self.bottom != self.rows - 1;
                self.row = if self.origin { self.top } else { 0 };
                self.column = 0;
                self.wrap_pending = false;
            }
            'h' if parameters == "?6" => self.origin = true,
            'l' if parameters == "?6" => self.origin = false,
            'h' | 'l' if parameters.starts_with('?') => {}
            'm' => {}
            other => panic!("unsupported CSI {parameters:?} {other:?}"),
        }
    }

    fn write_at(&mut self, row: usize, column: usize, text: &str) {
        let saved = (self.row, self.column, self.wrap_pending);
        self.row = row;
        self.column = column;
        self.wrap_pending = false;
        for character in text.chars() {
            self.print(character);
        }
        (self.row, self.column, self.wrap_pending) = saved;
    }

    fn print(&mut self, character: char) {
        if self.wrap_pending {
            self.column = 0;
            self.line_feed();
        }
        let width = character.width().unwrap_or(0);
        if width == 0 {
            if self.column != 0 {
                self.cells[self.row][self.column - 1].push(character);
            }
            return;
        }
        if self.column + width > self.columns {
            self.column = 0;
            self.line_feed();
        }
        self.cells[self.row][self.column] = character.to_string();
        for extra in 1..width {
            self.cells[self.row][self.column + extra] = "\0".to_owned();
        }
        self.column += width;
        self.wrap_pending = self.column == self.columns;
    }

    fn line_feed(&mut self) {
        self.wrap_pending = false;
        if self.row == self.bottom {
            let removed = self.line(self.top);
            let preserve = match self.policy {
                HistoryPolicy::FullScreenOnly => self.top == 0 && self.bottom == self.rows - 1,
                HistoryPolicy::TopAnchoredRegion => self.top == 0,
            };
            if preserve {
                self.history.push(removed);
            }
            for row in self.top..self.bottom {
                self.cells[row] = std::mem::take(&mut self.cells[row + 1]);
            }
            self.cells[self.bottom] = vec![String::new(); self.columns];
        } else {
            self.row = (self.row + 1).min(self.rows - 1);
        }
    }

    fn line(&self, row: usize) -> String {
        let mut output = String::new();
        for cell in &self.cells[row] {
            if cell != "\0" {
                output.push_str(cell);
            }
        }
        output.trim_end().to_owned()
    }
}

fn pair(parameters: &str, first_default: usize, second_default: usize) -> (usize, usize) {
    let mut values = parameters.split(';');
    let first = values
        .next()
        .filter(|value| !value.is_empty())
        .map_or(first_default, |value| value.parse().expect("numeric CSI"));
    let second = values
        .next()
        .filter(|value| !value.is_empty())
        .map_or(second_default, |value| value.parse().expect("numeric CSI"));
    assert!(values.next().is_none(), "unexpected extra CSI parameter");
    (first, second)
}

fn number(parameters: &str, default: usize) -> usize {
    if parameters.is_empty() {
        default
    } else {
        parameters.parse().expect("numeric CSI")
    }
}
