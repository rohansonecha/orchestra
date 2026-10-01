// line_edit.rs — Editing keys for orchestra's one-line inputs (the prompt
// and the rename box), matching what terminals and shells do.
//
// Terminals send many of these as control characters: VS Code / code-server
// turn Option+Delete into Ctrl+W and Cmd+Delete into Ctrl+U, and macOS
// terminals send Option+Delete as Alt+Backspace. A Ctrl or Alt combination
// that isn't an editing key is ignored, never typed as its letter.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// What a key did to the line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edit {
    /// The key edited the line or moved the cursor.
    Changed,
    /// Not an editing key; the caller decides (Enter, Esc, arrows, ...).
    NotHandled,
    /// A Ctrl/Alt combination with no editing meaning: swallowed.
    Ignored,
}

fn byte_at(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(i, _)| i).unwrap_or(s.len())
}

fn prev_word(chars: &[char], mut pos: usize) -> usize {
    while pos > 0 && chars[pos - 1].is_whitespace() {
        pos -= 1;
    }
    while pos > 0 && !chars[pos - 1].is_whitespace() {
        pos -= 1;
    }
    pos
}

fn next_word(chars: &[char], mut pos: usize) -> usize {
    while pos < chars.len() && chars[pos].is_whitespace() {
        pos += 1;
    }
    while pos < chars.len() && !chars[pos].is_whitespace() {
        pos += 1;
    }
    pos
}

/// Apply `key` to `input` with the cursor at `cursor` (a char index).
pub fn apply(input: &mut String, cursor: &mut usize, key: KeyEvent) -> Edit {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let chars: Vec<char> = input.chars().collect();
    let len = chars.len();
    *cursor = (*cursor).min(len);
    let delete = |input: &mut String, from: usize, to: usize| {
        let (a, b) = (byte_at(input, from), byte_at(input, to));
        input.replace_range(a..b, "");
    };
    match key.code {
        // Delete the previous word: Ctrl+W, Alt/Option+Backspace,
        // Ctrl+Backspace (some terminals send that one as Ctrl+H).
        KeyCode::Char('w') if ctrl => {
            let p = prev_word(&chars, *cursor);
            delete(input, p, *cursor);
            *cursor = p;
        }
        KeyCode::Backspace | KeyCode::Char('\u{7f}') | KeyCode::Char('\u{8}') if ctrl || alt => {
            let p = prev_word(&chars, *cursor);
            delete(input, p, *cursor);
            *cursor = p;
        }
        KeyCode::Char('h') if ctrl => {
            let p = prev_word(&chars, *cursor);
            delete(input, p, *cursor);
            *cursor = p;
        }
        // Alt+D / Ctrl+Delete: delete the next word.
        KeyCode::Char('d') if alt => {
            let n = next_word(&chars, *cursor);
            delete(input, *cursor, n);
        }
        KeyCode::Delete if ctrl || alt => {
            let n = next_word(&chars, *cursor);
            delete(input, *cursor, n);
        }
        KeyCode::Char('u') if ctrl => {
            delete(input, 0, *cursor);
            *cursor = 0;
        }
        KeyCode::Char('k') if ctrl => delete(input, *cursor, len),
        KeyCode::Char('a') if ctrl => *cursor = 0,
        KeyCode::Char('e') if ctrl => *cursor = len,
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = len,
        KeyCode::Left if ctrl || alt => *cursor = prev_word(&chars, *cursor),
        KeyCode::Right if ctrl || alt => *cursor = next_word(&chars, *cursor),
        KeyCode::Char('b') if alt => *cursor = prev_word(&chars, *cursor),
        KeyCode::Char('f') if alt => *cursor = next_word(&chars, *cursor),
        KeyCode::Char('b') if ctrl => *cursor = cursor.saturating_sub(1),
        KeyCode::Char('f') if ctrl => *cursor = (*cursor + 1).min(len),
        KeyCode::Backspace => {
            if *cursor == 0 {
                return Edit::Changed;
            }
            delete(input, *cursor - 1, *cursor);
            *cursor -= 1;
        }
        KeyCode::Delete => {
            if *cursor < len {
                delete(input, *cursor, *cursor + 1);
            }
        }
        KeyCode::Char(_) if ctrl || alt => return Edit::Ignored,
        KeyCode::Char(c) => {
            let at = byte_at(input, *cursor);
            input.insert(at, c);
            *cursor += 1;
        }
        _ => return Edit::NotHandled,
    }
    Edit::Changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }
    const C: KeyModifiers = KeyModifiers::CONTROL;
    const A: KeyModifiers = KeyModifiers::ALT;
    const N: KeyModifiers = KeyModifiers::NONE;

    fn run(start: &str, cursor: usize, key: KeyEvent) -> (String, usize, Edit) {
        let mut s = start.to_string();
        let mut c = cursor;
        let e = apply(&mut s, &mut c, key);
        (s, c, e)
    }

    #[test]
    fn word_and_line_deletes() {
        // Option+Delete in VS Code arrives as Ctrl+W.
        assert_eq!(run("fix the bug", 11, k(KeyCode::Char('w'), C)), ("fix the ".into(), 8, Edit::Changed));
        // Option+Delete in macOS terminals: Alt+Backspace.
        assert_eq!(run("fix the bug  ", 13, k(KeyCode::Backspace, A)).0, "fix the ");
        assert_eq!(run("fix the bug", 11, k(KeyCode::Backspace, C)).0, "fix the ");
        // Cmd+Delete in VS Code arrives as Ctrl+U.
        assert_eq!(run("fix the bug", 7, k(KeyCode::Char('u'), C)), (" bug".into(), 0, Edit::Changed));
        assert_eq!(run("fix the bug", 3, k(KeyCode::Char('k'), C)).0, "fix");
        assert_eq!(run("fix the bug", 4, k(KeyCode::Delete, C)).0, "fix  bug");
    }

    #[test]
    fn moves() {
        assert_eq!(run("fix the bug", 11, k(KeyCode::Char('a'), C)).1, 0);
        assert_eq!(run("fix the bug", 0, k(KeyCode::Char('e'), C)).1, 11);
        assert_eq!(run("fix the bug", 11, k(KeyCode::Left, A)).1, 8);
        assert_eq!(run("fix the bug", 0, k(KeyCode::Right, C)).1, 3);
        assert_eq!(run("fix", 3, k(KeyCode::Home, N)).1, 0);
    }

    #[test]
    fn ctrl_letters_are_never_typed() {
        for c in ['q', 'z', 'y', 'o', 'l', 'g'] {
            assert_eq!(run("ab", 2, k(KeyCode::Char(c), C)), ("ab".into(), 2, Edit::Ignored), "ctrl+{c}");
        }
        assert_eq!(run("ab", 2, k(KeyCode::Char('z'), A)).0, "ab");
        // Plain letters and unicode still type.
        assert_eq!(run("ab", 1, k(KeyCode::Char('é'), N)), ("aéb".into(), 2, Edit::Changed));
        assert_eq!(run("ab", 1, k(KeyCode::Char('X'), KeyModifiers::SHIFT)).0, "aXb");
    }

    #[test]
    fn plain_edits_and_passthrough() {
        assert_eq!(run("abc", 3, k(KeyCode::Backspace, N)), ("ab".into(), 2, Edit::Changed));
        assert_eq!(run("abc", 0, k(KeyCode::Delete, N)).0, "bc");
        assert_eq!(run("abc", 3, k(KeyCode::Enter, N)).2, Edit::NotHandled);
        assert_eq!(run("abc", 3, k(KeyCode::Up, N)).2, Edit::NotHandled);
        assert_eq!(run("abc", 3, k(KeyCode::Left, N)).2, Edit::NotHandled);
    }
}
