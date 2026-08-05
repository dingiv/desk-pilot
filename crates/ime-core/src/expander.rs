//! Text expander with variable substitution. Variables are trait-based so callers
//! inject platform-specific implementations (date, clipboard, etc.); unit tests
//! inject static strings.

/// Errors that can occur during expansion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandError {
    UnknownVariable(String),
}

impl std::fmt::Display for ExpandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExpandError::UnknownVariable(n) => write!(f, "unknown variable: ${n}"),
        }
    }
}

/// Trait for resolving expansion variables. Platform-agnostic — the fcitx5 / ibus /
/// familiar adapter implements this with real system calls; tests inject fakes.
pub trait VariableProvider: Send + Sync {
    /// Resolve a variable name (without the `$` prefix) to its value.
    /// Return `None` if the variable is unknown.
    fn resolve(&self, name: &str) -> Option<String>;

    /// Update a variable's value at runtime (default: unsupported no-op).
    /// The fcitx5 frontend pushes clipboard updates here (via the C ABI);
    /// `$CLIPBOARD` snippet templates then resolve to the fresh text.
    fn set(&self, _name: &str, _value: &str) {}
}

/// Default provider for the embedded engine (tests / mock / TUI): a live
/// `$DATE` and an empty clipboard. The fcitx5 frontend injects its own
/// provider with real clipboard values.
pub struct DefaultProvider;

impl VariableProvider for DefaultProvider {
    fn resolve(&self, name: &str) -> Option<String> {
        match name {
            "DATE" => Some(today_str()),
            "CLIPBOARD" => Some(String::new()),
            _ => None,
        }
    }
}

/// A provider for tests: fixed strings.
pub struct StaticProvider {
    pub date: String,
    pub clipboard: String,
}

impl VariableProvider for StaticProvider {
    fn resolve(&self, name: &str) -> Option<String> {
        match name {
            "DATE" => Some(self.date.clone()),
            "CLIPBOARD" => Some(self.clipboard.clone()),
            _ => None,
        }
    }
}

/// Today's date (YYYY-MM-DD). UTC-based, no chrono dep — Howard Hinnant's
/// civil-from-days conversion, so month lengths / leap years are exact.
pub fn today_str() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (y, m, d) = civil_from_days((secs / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Days since 1970-01-01 → (year, month, day). Howard Hinnant's
/// `civil_from_days` (reproduced here; ~10 lines, no dependency).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The expander holds a [`VariableProvider`] and expands template strings.
///
/// `#asr`/`#submit` no longer route through here — they are [`MagicFamily`]
/// members that read the shared voice buffer directly (see `magic/voice.rs`).
#[derive(Clone)]
pub struct Expander {
    provider: std::sync::Arc<dyn VariableProvider>,
}

impl Expander {
    /// `provider` is shared: the engine keeps a clone for `set_variable`
    /// (clipboard pushes), the expander reads it on every expansion.
    pub fn new(provider: std::sync::Arc<dyn VariableProvider>) -> Self {
        Expander { provider }
    }

    /// Expand a template string. Variables are `$NAME` or `${NAME}`.
    /// `$CURSOR` is replaced with an empty string (the caller is expected to
    /// record the cursor position — see [`expand_with_cursor`]).
    pub fn expand(&self, template: &str) -> Result<String, ExpandError> {
        self.expand_with_cursor(template).map(|(text, _)| text)
    }

    /// Expand a template, tracking where `$CURSOR` lands in the RESULT.
    ///
    /// Returns `(expanded, Some(byte_offset))` when the template contains
    /// `$CURSOR` — the offset is computed against the fully expanded text
    /// (variables before the marker may have variable length), so the caller
    /// can place the application caret exactly there after committing.
    /// `None` when the template has no cursor marker (caret stays at end).
    pub fn expand_with_cursor(
        &self,
        template: &str,
    ) -> Result<(String, Option<usize>), ExpandError> {
        let mut result = String::with_capacity(template.len());
        let mut cursor: Option<usize> = None;
        let mut chars = template.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '$' {
                let name: String = if let Some(&'{') = chars.peek() {
                    chars.next(); // consume '{'
                    let mut n = String::new();
                    for c in chars.by_ref() {
                        if c == '}' {
                            break;
                        }
                        n.push(c);
                    }
                    n
                } else if let Some(&c) = chars.peek() {
                    // Variable names must start with a letter or underscore.
                    // Digits after '$' → literal dollar (e.g. "$5").
                    if c.is_alphabetic() || c == '_' {
                        let mut n = String::new();
                        n.push(c);
                        chars.next();
                        while let Some(&c2) = chars.peek() {
                            if c2.is_alphanumeric() || c2 == '_' {
                                n.push(c2);
                                chars.next();
                            } else {
                                break;
                            }
                        }
                        n
                    } else {
                        // Non-letter after $ → literal
                        String::new()
                    }
                } else {
                    String::new() // '$' at end of string → literal
                };

                if name.is_empty() {
                    // Literal '$' — keep it, then continue with next char.
                    result.push('$');
                } else if name == "CURSOR" {
                    // Marker handled by the expander itself (not the provider):
                    // record where it lands in the expanded text, contribute nothing.
                    cursor = Some(result.len());
                } else {
                    match self.provider.resolve(&name) {
                        Some(value) => result.push_str(&value),
                        None => return Err(ExpandError::UnknownVariable(name)),
                    }
                }
            } else {
                result.push(ch);
            }
        }

        Ok((result, cursor))
    }

    /// Return the byte position of `$CURSOR` in the template (for the caller to
    /// compute where the cursor should go after expansion). Returns `None` if
    /// there is no cursor variable.
    pub fn cursor_pos_in_template(template: &str) -> Option<usize> {
        template.find("$CURSOR")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> StaticProvider {
        StaticProvider {
            date: "2026-07-23".into(),
            clipboard: "clipboard_content".into(),
        }
    }

    fn expander() -> Expander {
        Expander::new(std::sync::Arc::new(provider()))
    }

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(expander().expand("hello world").unwrap(), "hello world");
    }

    #[test]
    fn simple_variable() {
        assert_eq!(expander().expand("Today is $DATE").unwrap(), "Today is 2026-07-23");
    }

    #[test]
    fn braced_variable() {
        assert_eq!(
            expander().expand("Clip: ${CLIPBOARD}").unwrap(),
            "Clip: clipboard_content"
        );
    }

    #[test]
    fn cursor_variable_removed() {
        assert_eq!(
            expander().expand("Hello $CURSOR world").unwrap(),
            "Hello  world"
        );
    }

    #[test]
    fn unknown_variable_errors() {
        assert_eq!(
            expander().expand("$UNKNOWN"),
            Err(ExpandError::UnknownVariable("UNKNOWN".into()))
        );
    }

    #[test]
    fn literal_dollar() {
        assert_eq!(expander().expand("Cost: $5").unwrap(), "Cost: $5");
        assert_eq!(expander().expand("$").unwrap(), "$");
    }

    #[test]
    fn cursor_position() {
        let t = "Hello $CURSOR World";
        let pos = Expander::cursor_pos_in_template(t);
        assert_eq!(pos, Some(6)); // byte offset of '$'
    }

    #[test]
    fn multiline_expansion() {
        let t = "Hi,\n\n$CLIPBOARD\n\nBest,\n$DATE";
        let result = expander().expand(t).unwrap();
        assert!(result.contains("clipboard_content"));
        assert!(result.contains("2026-07-23"));
    }

    #[test]
    fn expand_tracks_cursor_offset() {
        let (text, cur) = expander().expand_with_cursor("Hello $CURSOR World").unwrap();
        assert_eq!(text, "Hello  World");
        assert_eq!(cur, Some(6)); // byte offset of the marker in the RESULT
    }

    #[test]
    fn cursor_offset_counts_expanded_variables_before_it() {
        // $DATE resolves to 10 bytes; " " is 1 → marker sits at byte 11.
        let (text, cur) = expander().expand_with_cursor("$DATE $CURSOR done").unwrap();
        assert_eq!(cur, Some(11), "text={text:?}");
        assert!(text.starts_with("2026-07-23 "));
    }

    #[test]
    fn no_cursor_returns_none() {
        let (text, cur) = expander().expand_with_cursor("plain $DATE").unwrap();
        assert_eq!(cur, None);
        assert_eq!(text, "plain 2026-07-23");
    }

    #[test]
    fn default_provider_has_live_date_and_empty_clipboard() {
        let d = DefaultProvider;
        let date = d.resolve("DATE").expect("DATE resolves");
        assert_eq!(date.len(), 10, "YYYY-MM-DD");
        assert_eq!(d.resolve("CLIPBOARD"), Some(String::new()));
        assert_eq!(d.resolve("UNKNOWN"), None);
    }

    #[test]
    fn set_updates_provider_value_for_later_expansion() {
        // The fcitx5 frontend pushes clipboard changes via `set`; the next
        // expansion of $CLIPBOARD must see the fresh text.
        #[derive(Default)]
        struct Mutable {
            clipboard: std::sync::Mutex<String>,
        }
        impl VariableProvider for Mutable {
            fn resolve(&self, name: &str) -> Option<String> {
                match name {
                    "CLIPBOARD" => Some(self.clipboard.lock().unwrap().clone()),
                    _ => None,
                }
            }
            fn set(&self, name: &str, value: &str) {
                if name == "CLIPBOARD" {
                    *self.clipboard.lock().unwrap() = value.to_string();
                }
            }
        }

        let provider: std::sync::Arc<dyn VariableProvider> =
            std::sync::Arc::new(Mutable::default());
        let e = Expander::new(std::sync::Arc::clone(&provider));

        assert_eq!(e.expand("$CLIPBOARD").unwrap(), "");
        provider.set("CLIPBOARD", "hello");
        assert_eq!(e.expand("$CLIPBOARD").unwrap(), "hello");
        e.provider.set("CLIPBOARD", "world"); // through the expander's own Arc
        assert_eq!(e.expand("$CLIPBOARD").unwrap(), "world");
    }

    #[test]
    fn today_str_is_well_formed() {
        let s = today_str();
        assert_eq!(s.len(), 10);
        assert_eq!(s.as_bytes()[4], b'-');
        assert_eq!(s.as_bytes()[7], b'-');
        let y: u32 = s[0..4].parse().unwrap();
        let m: u32 = s[5..7].parse().unwrap();
        let d: u32 = s[8..10].parse().unwrap();
        assert!(y >= 2026, "current year");
        assert!((1..=12).contains(&m));
        assert!((1..=31).contains(&d));
    }

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        assert_eq!(civil_from_days(364), (1970, 12, 31));
        assert_eq!(civil_from_days(365), (1971, 1, 1));
        // 2000 is a leap year: 2000-02-29 exists (days since epoch 11016).
        assert_eq!(civil_from_days(11016), (2000, 2, 29));
        assert_eq!(civil_from_days(11017), (2000, 3, 1));
        // 2024 is also a leap year.
        assert_eq!(civil_from_days(19781), (2024, 2, 28));
        assert_eq!(civil_from_days(19782), (2024, 2, 29));
        assert_eq!(civil_from_days(19783), (2024, 3, 1));
    }
}
