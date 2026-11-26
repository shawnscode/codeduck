use rust_i18n::t;
use std::cell::RefCell;

pub const DEFAULT_LOCALE: &str = "en";

/// Return all compiled locale codes discovered from `locales/`, with `"en"` first.
pub fn available_locales() -> Vec<String> {
    let mut locales: Vec<String> = rust_i18n::available_locales!()
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    locales.sort_by(|a, b| match (a == DEFAULT_LOCALE, b == DEFAULT_LOCALE) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => a.cmp(b),
    });
    locales
}

/// Resolve a user-supplied locale string (exact code, normalized `_`/`-`, or `[meta].aliases`)
/// to a canonical compiled locale code in `locales/`.
pub fn resolve_locale(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let norm = trimmed.to_ascii_lowercase().replace('_', "-");
    let available = available_locales();

    // 1. Direct match against locale code
    for code in &available {
        if code.to_ascii_lowercase().replace('_', "-") == norm {
            return Some(code.clone());
        }
    }

    // 2. Match against aliases declared inside each `locales/<code>.toml` `[meta].aliases`
    for code in &available {
        let aliases = t!("meta.aliases", locale = code.as_str());
        for alias in aliases.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if alias == trimmed || alias.to_ascii_lowercase().replace('_', "-") == norm {
                return Some(code.clone());
            }
        }
    }

    None
}

pub fn display_name(code: &str) -> String {
    let resolved = resolve_locale(code).unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    t!("meta.display_name", locale = resolved.as_str()).into_owned()
}

pub fn next_locale(current: &str, forward: bool) -> String {
    let list = available_locales();
    if list.is_empty() {
        return DEFAULT_LOCALE.to_string();
    }
    let idx = list
        .iter()
        .position(|c| c.eq_ignore_ascii_case(current))
        .unwrap_or(0);
    let next = if forward {
        (idx + 1) % list.len()
    } else if idx == 0 {
        list.len() - 1
    } else {
        idx - 1
    };
    list[next].clone()
}

pub fn available_summary() -> String {
    available_locales().join(" | ")
}

pub fn format_written_report(locale: &str, path: &str) -> String {
    t!("export.written_to", locale = locale, path = path).into_owned()
}

thread_local! {
    static ACTIVE_LOCALE: RefCell<String> = const { RefCell::new(String::new()) };
}

pub fn run_with_locale<T>(locale: &str, f: impl FnOnce() -> T) -> T {
    struct Guard(String);
    impl Drop for Guard {
        fn drop(&mut self) {
            let prev = std::mem::take(&mut self.0);
            ACTIVE_LOCALE.with(|cell| *cell.borrow_mut() = prev);
        }
    }
    let canonical = resolve_locale(locale).unwrap_or_else(|| DEFAULT_LOCALE.to_string());
    let prev = ACTIVE_LOCALE.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), canonical));
    let _guard = Guard(prev);
    f()
}

pub fn current_thread_locale() -> String {
    ACTIVE_LOCALE.with(|cell| {
        let s = cell.borrow();
        if s.is_empty() {
            DEFAULT_LOCALE.to_string()
        } else {
            s.clone()
        }
    })
}
