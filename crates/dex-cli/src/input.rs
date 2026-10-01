//! Reading the user's line, and noticing Ctrl-C.
//!
//! One task owns the keyboard, so the main loop never has to poll it and the
//! two kinds of interrupt can be told apart:
//!
//! - a bare Enter is a line of input,
//! - Ctrl-C **during a turn** asks the runtime to cancel it,
//! - Ctrl-C **at the prompt** quits, because there is nothing to cancel.
//!
//! That distinction is what makes Ctrl-C feel like it does what you expect.

use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// What the keyboard produced.
#[derive(Debug, PartialEq, Eq)]
pub enum Input {
    /// A submitted line, trimmed.
    Line(String),
    /// Ctrl-C.
    Interrupt,
    /// Ctrl-D on an empty line.
    Eof,
}

pub struct Prompt {
    line: String,
    /// Where the cursor sits within `line`.
    cursor: usize,
}

impl Default for Prompt {
    fn default() -> Self {
        Self::new()
    }
}

impl Prompt {
    pub fn new() -> Self {
        Self {
            line: String::new(),
            cursor: 0,
        }
    }

    pub fn buffer(&self) -> &str {
        &self.line
    }

    pub fn clear(&mut self) {
        self.line.clear();
        self.cursor = 0;
    }

    /// Apply one key, returning an input when the line is submitted or
    /// interrupted.
    pub fn apply(&mut self, key: KeyEvent) -> Option<Input> {
        // Some terminals report both press and release; acting on the press is
        // enough.
        if key.kind == KeyEventKind::Release {
            return None;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('c'), KeyModifiers::NONE) => {
                // Ctrl-C is the interrupt; a bare `c` is ordinary input, so it
                // only matches when CONTROL is held or the terminal has no
                // modifier support.
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    return Some(Input::Interrupt);
                }
                self.push('c');
            }
            (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                if self.line.is_empty() {
                    return Some(Input::Eof);
                }
                self.push('d');
            }
            (KeyCode::Enter, _) => {
                let line = std::mem::take(&mut self.line).trim().to_string();
                self.cursor = 0;
                if line.is_empty() {
                    return None;
                }
                return Some(Input::Line(line));
            }
            // `remove` takes a byte index, while the cursor counts characters,
            // so every edit converts. Using the cursor directly would split a
            // multi-byte character and panic.
            (KeyCode::Backspace, _) => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let index = char_index(&self.line, self.cursor);
                    self.line.remove(index);
                }
            }
            (KeyCode::Delete, _) => {
                if self.cursor < self.chars() {
                    let index = char_index(&self.line, self.cursor);
                    self.line.remove(index);
                }
            }
            (KeyCode::Left, _) => self.cursor = self.cursor.saturating_sub(1),
            (KeyCode::Right, _) => {
                if self.cursor < self.chars() {
                    self.cursor += 1;
                }
            }
            (KeyCode::Home, _) => self.cursor = 0,
            (KeyCode::End, _) => self.cursor = self.line.chars().count(),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => self.clear(),
            (KeyCode::Char(c), _) => self.push(c),
            _ => {}
        }
        None
    }

    /// How many characters the line holds.
    fn chars(&self) -> usize {
        self.line.chars().count()
    }

    fn push(&mut self, c: char) {
        // Work in character indices so a multi-byte paste does not split.
        let index = char_index(&self.line, self.cursor);
        self.line.insert(index, c);
        self.cursor += 1;
    }
}

fn char_index(text: &str, cursor: usize) -> usize {
    text.char_indices()
        .nth(cursor)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

/// Wait for the next key. Returns `None` if the input stream ends.
///
/// Crossterm's own reader is blocking, so it is bridged through
/// `EventStream`, which is async and integrates with the runtime the rest of
/// the frontend already uses.
pub async fn next_key() -> Option<KeyEvent> {
    let mut stream = EventStream::new();
    loop {
        // crossterm's stream yields `Option<Result<..>>`: the outer is the end
        // of input, the inner an error or the event itself.
        match futures::StreamExt::next(&mut stream).await {
            Some(Ok(TermEvent::Key(key))) => return Some(key),
            // Resize and focus changes are not input; keep waiting.
            Some(Ok(_)) => continue,
            Some(Err(_)) | None => return None,
        }
    }
}

/// Put the terminal into raw mode for line editing, restoring it afterwards.
///
/// The guard restores on drop as well as on an explicit [`Prompt::restore`],
/// so a panic mid-turn still leaves a usable terminal behind.
pub struct RawMode;

impl RawMode {
    pub fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        Ok(Self)
    }

    pub fn restore() -> std::io::Result<()> {
        crossterm::terminal::disable_raw_mode()
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = Self::restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn typing_and_submitting_a_line() {
        let mut p = Prompt::new();
        for c in "hi".chars() {
            assert!(p.apply(key(KeyCode::Char(c))).is_none());
        }
        assert_eq!(p.buffer(), "hi");
        assert_eq!(p.apply(key(KeyCode::Enter)), Some(Input::Line("hi".into())));
        assert_eq!(p.buffer(), "", "the line is consumed on submit");
    }

    #[test]
    fn an_empty_line_is_not_an_input() {
        let mut p = Prompt::new();
        assert_eq!(p.apply(key(KeyCode::Enter)), None);
        assert!(p.apply(key(KeyCode::Char(' '))).is_none());
        assert_eq!(p.apply(key(KeyCode::Enter)), None);
    }

    #[test]
    fn ctrl_c_is_an_interrupt_not_a_letter() {
        let mut p = Prompt::new();
        assert_eq!(p.apply(ctrl('c')), Some(Input::Interrupt));
        assert_eq!(p.buffer(), "");
    }

    #[test]
    fn ctrl_d_on_an_empty_line_is_eof() {
        let mut p = Prompt::new();
        assert_eq!(p.apply(ctrl('d')), Some(Input::Eof));
    }

    #[test]
    fn backspace_edits_the_line() {
        let mut p = Prompt::new();
        for c in "abc".chars() {
            p.apply(key(KeyCode::Char(c)));
        }
        p.apply(key(KeyCode::Backspace));
        assert_eq!(p.buffer(), "ab");
        // Backspace on an empty line is not an error.
        p.apply(key(KeyCode::Backspace));
        p.apply(key(KeyCode::Backspace));
        p.apply(key(KeyCode::Backspace));
        assert_eq!(p.buffer(), "");
    }

    #[test]
    fn the_cursor_moves_within_the_line() {
        let mut p = Prompt::new();
        for c in "abc".chars() {
            p.apply(key(KeyCode::Char(c)));
        }
        p.apply(key(KeyCode::Home));
        p.apply(key(KeyCode::Char('x')));
        assert_eq!(p.buffer(), "xabc");
        p.apply(key(KeyCode::End));
        p.apply(key(KeyCode::Char('y')));
        assert_eq!(p.buffer(), "xabcy");
    }

    #[test]
    fn ctrl_u_clears_the_line() {
        let mut p = Prompt::new();
        for c in "discard me".chars() {
            p.apply(key(KeyCode::Char(c)));
        }
        p.apply(ctrl('u'));
        assert_eq!(p.buffer(), "");
    }

    #[test]
    fn a_multi_byte_character_survives_editing() {
        let mut p = Prompt::new();
        p.apply(key(KeyCode::Char('\u{e9}')));
        p.apply(key(KeyCode::Char('!')));
        assert_eq!(p.buffer(), "\u{e9}!");
        p.apply(key(KeyCode::Backspace));
        assert_eq!(p.buffer(), "\u{e9}");
    }

    #[test]
    fn a_release_event_is_ignored() {
        // Some terminals send both; acting on the release would double it.
        let mut p = Prompt::new();
        let mut k = key(KeyCode::Char('a'));
        k.kind = KeyEventKind::Release;
        p.apply(k);
        assert_eq!(p.buffer(), "");
    }

    #[test]
    fn submitted_lines_are_trimmed() {
        let mut p = Prompt::new();
        for c in "  hello  ".chars() {
            p.apply(key(KeyCode::Char(c)));
        }
        assert_eq!(p.apply(key(KeyCode::Enter)), Some(Input::Line("hello".into())));
    }
}