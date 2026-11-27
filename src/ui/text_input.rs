use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthStr;

/// Clamp byte cursor cleanly onto a valid UTF-8 character boundary within `buf`.
pub fn clamp_cursor(buf: &str, cursor: usize) -> usize {
    let mut c = cursor.min(buf.len());
    while c > 0 && !buf.is_char_boundary(c) {
        c -= 1;
    }
    c
}

fn is_cluster_extender(ch: char) -> bool {
    matches!(
        ch,
        '\u{0300}'..='\u{036F}'
            | '\u{1AB0}'..='\u{1AFF}'
            | '\u{1DC0}'..='\u{1DFF}'
            | '\u{20D0}'..='\u{20FF}'
            | '\u{FE20}'..='\u{FE2F}'
            | '\u{FE00}'..='\u{FE0F}'
            | '\u{E0100}'..='\u{E01EF}'
            | '\u{200D}'
            | '\u{1F3FB}'..='\u{1F3FF}'
    )
}

fn next_cluster_boundary(buf: &str, cursor: usize) -> usize {
    let mut pos = clamp_cursor(buf, cursor);
    if pos >= buf.len() {
        return buf.len();
    }
    let Some(first) = buf[pos..].chars().next() else {
        return buf.len();
    };
    pos += first.len_utf8();
    if first == '\r' && buf[pos..].starts_with('\n') {
        return pos + 1;
    }
    while let Some(next) = buf[pos..].chars().next() {
        if !is_cluster_extender(next) {
            break;
        }
        pos += next.len_utf8();
        if next == '\u{200D}' {
            if let Some(joined) = buf[pos..].chars().next() {
                pos += joined.len_utf8();
            }
        }
    }
    pos
}

fn prev_cluster_boundary(buf: &str, cursor: usize) -> usize {
    let mut pos = clamp_cursor(buf, cursor);
    if pos == 0 {
        return 0;
    }
    if buf[..pos].ends_with("\r\n") {
        return pos - 2;
    }
    loop {
        let Some((idx, ch)) = buf[..pos].char_indices().last() else {
            return 0;
        };
        pos = idx;
        if is_cluster_extender(ch) {
            continue;
        }
        // If this base char was joined via a preceding ZWJ, step back over the ZWJ cluster too.
        if pos > 0 && buf[..pos].ends_with('\u{200D}') {
            pos -= '\u{200D}'.len_utf8();
            continue;
        }
        break;
    }
    pos
}

fn prev_char_boundary(buf: &str, cursor: usize) -> usize {
    let c = clamp_cursor(buf, cursor);
    if c == 0 {
        return 0;
    }
    buf[..c].char_indices().last().map(|(i, _)| i).unwrap_or(0)
}

fn split_clusters(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let next = next_cluster_boundary(s, i);
        if next <= i {
            break;
        }
        out.push(&s[i..next]);
        i = next;
    }
    out
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
            *cursor = prev_cluster_boundary(buf, *cursor);
            false
        }
        KeyCode::Right => {
            *cursor = next_cluster_boundary(buf, *cursor);
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
                let prev = prev_cluster_boundary(buf, *cursor);
                buf.drain(prev..*cursor);
                *cursor = prev;
                true
            } else {
                false
            }
        }
        KeyCode::Delete => {
            if *cursor < buf.len() {
                let next = next_cluster_boundary(buf, *cursor);
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

/// Format text with an inline `|` cursor marker horizontally scrolled around `cursor`,
/// returning `(formatted_string, cursor_visual_col_offset)`.
pub fn format_with_cursor_and_col(buf: &str, cursor: usize, max_cols: usize) -> (String, u16) {
    let pos = clamp_cursor(buf, cursor);
    let before = &buf[..pos];
    let after = &buf[pos..];
    if max_cols == 0 {
        return (String::new(), 0);
    }
    if max_cols == 1 {
        return ("|".to_string(), 0);
    }

    let before_w = UnicodeWidthStr::width(before);
    let after_w = UnicodeWidthStr::width(after);
    if before_w + 1 + after_w <= max_cols {
        return (format!("{}|{}", before, after), before_w as u16);
    }

    let rem = max_cols.saturating_sub(1); // cols aside from '|'
    let mut kept_before = String::new();
    let mut w_before = 0usize;
    let mut truncated_left = false;
    if before_w > 0 {
        let desired_before = if after_w == 0 {
            rem
        } else {
            (rem / 2).max(rem.saturating_sub(after_w))
        };
        if before_w <= desired_before {
            kept_before.push_str(before);
            w_before = before_w;
        } else {
            let left_budget = if rem >= 2 {
                desired_before.saturating_sub(1)
            } else {
                0
            };
            let clusters = split_clusters(before);
            let mut rev_kept = Vec::new();
            for cl in clusters.into_iter().rev() {
                let cw = UnicodeWidthStr::width(cl);
                if w_before + cw > left_budget {
                    truncated_left = true;
                    break;
                }
                w_before += cw;
                rev_kept.push(cl);
            }
            if rev_kept.len() < split_clusters(before).len() {
                truncated_left = true;
            }
            for cl in rev_kept.into_iter().rev() {
                kept_before.push_str(cl);
            }
        }
    }

    let prefix = if truncated_left && rem >= 2 { "…" } else { "" };
    let prefix_w = UnicodeWidthStr::width(prefix);
    let used = prefix_w + w_before + 1;
    let avail_after = max_cols.saturating_sub(used);

    let mut kept_after = String::new();
    let mut w_after = 0usize;
    let mut truncated_right = false;
    if avail_after > 0 && !after.is_empty() {
        if after_w <= avail_after {
            kept_after.push_str(after);
        } else {
            let right_budget = if avail_after >= 2 {
                avail_after.saturating_sub(1)
            } else {
                0
            };
            for cl in split_clusters(after) {
                let cw = UnicodeWidthStr::width(cl);
                if w_after + cw > right_budget {
                    truncated_right = true;
                    break;
                }
                w_after += cw;
                kept_after.push_str(cl);
            }
            if kept_after.len() < after.len() {
                truncated_right = true;
            }
        }
    } else if !after.is_empty() {
        truncated_right = true;
    }

    let suffix = if truncated_right && avail_after >= 1 {
        "…"
    } else {
        ""
    };
    let cursor_col = (prefix_w + w_before) as u16;
    (format!("{}{}|{}{}", prefix, kept_before, kept_after, suffix), cursor_col)
}

/// Format text with an inline `|` cursor marker, horizontally scrolled around cursor.
pub fn format_with_cursor(buf: &str, cursor: usize, max_cols: usize) -> String {
    format_with_cursor_and_col(buf, cursor, max_cols).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cluster_navigation_and_backspace_cjk_and_combining() {
        let mut s = String::from("a\u{0301}中😀b");
        let mut cur = s.len();
        assert!(!handle_key(&mut s, &mut cur, KeyEvent::from(KeyCode::Left)));
        assert_eq!(&s[..cur], "a\u{0301}中😀");
        assert!(!handle_key(&mut s, &mut cur, KeyEvent::from(KeyCode::Left)));
        assert_eq!(&s[..cur], "a\u{0301}中");
        assert!(!handle_key(&mut s, &mut cur, KeyEvent::from(KeyCode::Left)));
        assert_eq!(&s[..cur], "a\u{0301}");
        assert!(handle_key(&mut s, &mut cur, KeyEvent::from(KeyCode::Backspace)));
        assert_eq!(s, "中😀b");
        assert_eq!(cur, 0);
        assert!(!handle_key(&mut s, &mut cur, KeyEvent::from(KeyCode::Right)));
        assert_eq!(&s[..cur], "中");
    }

    #[test]
    fn format_with_cursor_and_col_clamps_narrow_and_marks_right_ellipsis() {
        let s = "中文输入法测试文本超过宽度";
        let (out, col) = format_with_cursor_and_col(s, 0, 7);
        assert!(out.ends_with('…'), "expected right ellipsis in `{}`", out);
        assert!(UnicodeWidthStr::width(out.as_str()) <= 7);
        assert_eq!(col, 0);

        let (mid, mid_col) = format_with_cursor_and_col("abcdef1234567890", 10, 6);
        assert!(UnicodeWidthStr::width(mid.as_str()) <= 6);
        assert!(mid.contains('|'));
        assert!((mid_col as usize) < 6);
    }
}
