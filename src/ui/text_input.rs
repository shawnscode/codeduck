use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Clamp byte cursor cleanly onto a valid UTF-8 character boundary within `buf`.
pub fn clamp_cursor(buf: &str, cursor: usize) -> usize {
    let mut c = cursor.min(buf.len());
    while c > 0 && !buf.is_char_boundary(c) {
        c -= 1;
    }
    c
}

fn prev_char_boundary(buf: &str, cursor: usize) -> usize {
    let c = clamp_cursor(buf, cursor);
    if c == 0 {
        return 0;
    }
    buf[..c].char_indices().last().map(|(i, _)| i).unwrap_or(0)
}

fn next_char_boundary(buf: &str, cursor: usize) -> usize {
    let c = clamp_cursor(buf, cursor);
    if c >= buf.len() {
        return buf.len();
    }
    c + buf[c..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(0)
}

pub fn insert_clean_str(buf: &mut String, cursor: &mut usize, raw: &str) {
    let cleaned: String = raw
        .chars()
        .map(|ch| match ch {
            '\r' | '\n' | '\t' => ' ',
            _ if ch.is_control() => ' ',
            _ => ch,
        })
        .collect();
    if cleaned.is_empty() {
        return;
    }
    let pos = clamp_cursor(buf, *cursor);
    buf.insert_str(pos, &cleaned);
    *cursor = pos + cleaned.len();
}

/// Handle cursor movement and editing keys inside a single-line text field.
/// Returns `true` if the buffer content itself was modified.
pub fn handle_key(buf: &mut String, cursor: &mut usize, key: KeyEvent) -> bool {
    *cursor = clamp_cursor(buf, *cursor);

    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('a') => {
                *cursor = 0;
                return false;
            }
            KeyCode::Char('e') => {
                *cursor = buf.len();
                return false;
            }
            KeyCode::Char('u') => {
                if *cursor > 0 {
                    buf.drain(..*cursor);
                    *cursor = 0;
                    return true;
                }
                return false;
            }
            KeyCode::Char('w') => {
                if *cursor > 0 {
                    let mut start = *cursor;
                    while start > 0 {
                        let prev = prev_char_boundary(buf, start);
                        let ch = buf[prev..start].chars().next().unwrap_or(' ');
                        if !ch.is_whitespace() {
                            break;
                        }
                        start = prev;
                    }
                    while start > 0 {
                        let prev = prev_char_boundary(buf, start);
                        let ch = buf[prev..start].chars().next().unwrap_or(' ');
                        if ch.is_whitespace() {
                            break;
                        }
                        start = prev;
                    }
                    buf.drain(start..*cursor);
                    *cursor = start;
                    return true;
                }
                return false;
            }
            _ => return false,
        }
    }

    match key.code {
        KeyCode::Left => {
            *cursor = prev_char_boundary(buf, *cursor);
            false
        }
        KeyCode::Right => {
            *cursor = next_char_boundary(buf, *cursor);
            false
        }
        KeyCode::Home => {
            *cursor = 0;
            false
        }
        KeyCode::End => {
            *cursor = buf.len();
            false
        }
        KeyCode::Backspace => {
            if *cursor > 0 {
                let prev = prev_char_boundary(buf, *cursor);
                buf.drain(prev..*cursor);
                *cursor = prev;
                true
            } else {
                false
            }
        }
        KeyCode::Delete => {
            if *cursor < buf.len() {
                let next = next_char_boundary(buf, *cursor);
                buf.drain(*cursor..next);
                true
            } else {
                false
            }
        }
        KeyCode::Char(c) if !c.is_control() => {
            let pos = *cursor;
            buf.insert(pos, c);
            *cursor = pos + c.len_utf8();
            true
        }
        _ => false,
    }
}

/// Format text with an inline `|` cursor marker, horizontally scrolled around cursor if `max_cols > 0`.
pub fn format_with_cursor(buf: &str, cursor: usize, max_cols: usize) -> String {
    let pos = clamp_cursor(buf, cursor);
    let before = &buf[..pos];
    let after = &buf[pos..];
    if max_cols < 8 {
        return format!("{}|{}", before, after);
    }

    let before_w = UnicodeWidthStr::width(before);
    let after_w = UnicodeWidthStr::width(after);
    if before_w + 1 + after_w <= max_cols {
        return format!("{}|{}", before, after);
    }

    let target_before = max_cols.saturating_sub(4) / 2;
    let mut kept_before = String::new();
    let mut w_before = 0usize;
    let mut truncated_left = false;
    for ch in before.chars().rev() {
        let cw = ch.width().unwrap_or(0);
        if w_before + cw > target_before {
            truncated_left = true;
            break;
        }
        w_before += cw;
        kept_before.push(ch);
    }
    let kept_before: String = kept_before.chars().rev().collect();

    let prefix = if truncated_left { "…" } else { "" };
    let used = UnicodeWidthStr::width(prefix) + w_before + 1;
    let avail_after = max_cols.saturating_sub(used);
    let mut kept_after = String::new();
    let mut w_after = 0usize;
    for ch in after.chars() {
        let cw = ch.width().unwrap_or(0);
        if w_after + cw > avail_after {
            break;
        }
        w_after += cw;
        kept_after.push(ch);
    }

    format!("{}{}|{}", prefix, kept_before, kept_after)
}
