//! Text expander with variable substitution. Variables are trait-based so callers
//! inject platform-specific implementations (date, clipboard, voice buffer, etc.);
//! unit tests inject static strings.

use std::sync::Mutex;

use crate::asr_buffer::AsrBuffer;

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
}

/// A provider for tests and Phase 1 (no real clipboard integration yet).
pub struct StaticProvider {
    pub date: String,
    pub clipboard: String,
}

impl VariableProvider for StaticProvider {
    fn resolve(&self, name: &str) -> Option<String> {
        match name {
            "DATE" => Some(self.date.clone()),
            "CLIPBOARD" => Some(self.clipboard.clone()),
            "CURSOR" => Some("".into()), // cursor placeholder — removed on expand
            _ => None,
        }
    }
}

/// The expander holds a [`VariableProvider`] and expands template strings.
///
/// It also holds an optional [`AsrBuffer`] reference. When the template is the
/// special token `__ASR_BUFFER__`, the expander returns the current voice buffer
/// content (or an empty string if no voice data has been received).
pub struct Expander {
    provider: std::sync::Arc<dyn VariableProvider>,
    /// Optional voice buffer for `#asr` command resolution. Set after
    /// engine construction (once the SSE client is running).
    asr: Mutex<Option<std::sync::Arc<AsrBuffer>>>,
}

impl Clone for Expander {
    fn clone(&self) -> Self {
        Expander {
            provider: self.provider.clone(),
            asr: Mutex::new(self.asr.lock().unwrap().clone()),
        }
    }
}

impl Expander {
    pub fn new(provider: Box<dyn VariableProvider>) -> Self {
        Expander {
            provider: std::sync::Arc::from(provider),
            asr: Mutex::new(None),
        }
    }

    /// Attach the voice buffer so `#asr` → `__ASR_BUFFER__` resolves to live
    /// voice recognition text.
    pub fn set_asr_buffer(&self, buf: std::sync::Arc<AsrBuffer>) {
        *self.asr.lock().unwrap() = Some(buf);
    }

    /// Expand a template string. Variables are `$NAME` or `${NAME}`.
    /// `$CURSOR` is replaced with an empty string (the caller is expected to
    /// record the cursor position from the pre-expansion string length).
    ///
    /// Special tokens:
    /// - `__ASR_BUFFER__` — replaced with the current voice buffer content.
    ///   Returns a **preview** placeholder `"语音识别中..."` when the buffer
    ///   is empty (no voice data received yet). Uses `snapshot()` so the same
    ///   text persists until the next utterance arrives.
    /// - `__ASR_SUBMIT__` — consumed the buffer with `take()`. Returns empty
    ///   string if nothing is available. Used by the `#submit` command for
    ///   one-shot commit.
    pub fn expand(&self, template: &str) -> Result<String, ExpandError> {
        // ── Special token: voice buffer (preview) ──
        if template == "__ASR_BUFFER__" {
            let guard = self.asr.lock().unwrap();
            let text = guard.as_ref().map(|b| b.snapshot()).unwrap_or_default();
            if text.is_empty() {
                return Ok("语音识别中...".into());
            }
            return Ok(text);
        }

        // ── Special token: voice buffer (one-shot consume for #submit) ──
        // Uses snapshot() like __ASR_BUFFER__ — expand() may be called twice
        // (match time + commit time). The SSE client overwrites the buffer on
        // each new utterance so the same text persists until refreshed.
        if template == "__ASR_SUBMIT__" {
            let guard = self.asr.lock().unwrap();
            let text = guard.as_ref().map(|b| b.snapshot()).unwrap_or_default();
            if text.is_empty() {
                return Ok("".into()); // #submit with no data = empty (not preview)
            }
            return Ok(text);
        }

        let mut result = String::with_capacity(template.len());
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

        Ok(result)
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
        Expander::new(Box::new(provider()))
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
    fn asr_buffer_token_without_buffer_returns_preview() {
        // No AsrBuffer attached → __ASR_BUFFER__ returns preview placeholder.
        assert_eq!(expander().expand("__ASR_BUFFER__").unwrap(), "语音识别中...");
    }

    #[test]
    fn asr_buffer_token_empty_buffer_returns_preview() {
        use crate::asr_buffer::AsrBuffer;
        let buf = std::sync::Arc::new(AsrBuffer::new());
        // Buffer exists but is empty → preview.
        let mut exp = expander();
        exp.set_asr_buffer(buf);
        assert_eq!(exp.expand("__ASR_BUFFER__").unwrap(), "语音识别中...");
    }

    #[test]
    fn asr_buffer_token_with_buffer() {
        use crate::asr_buffer::AsrBuffer;
        let buf = std::sync::Arc::new(AsrBuffer::new());
        buf.update("今天天气不错");
        let mut exp = expander();
        exp.set_asr_buffer(buf);
        assert_eq!(exp.expand("__ASR_BUFFER__").unwrap(), "今天天气不错");
    }

    #[test]
    fn asr_submit_token_returns_text() {
        use crate::asr_buffer::AsrBuffer;
        let buf = std::sync::Arc::new(AsrBuffer::new());
        buf.update("提交测试文本");
        let mut exp = expander();
        exp.set_asr_buffer(buf);
        // __ASR_SUBMIT__ uses snapshot(), not take() — survives multiple calls.
        assert_eq!(exp.expand("__ASR_SUBMIT__").unwrap(), "提交测试文本");
        assert_eq!(exp.expand("__ASR_SUBMIT__").unwrap(), "提交测试文本");
    }

    #[test]
    fn asr_submit_without_buffer_returns_empty() {
        assert_eq!(expander().expand("__ASR_SUBMIT__").unwrap(), "");
    }

    #[test]
    fn asr_submit_empty_buffer_returns_empty() {
        use crate::asr_buffer::AsrBuffer;
        let buf = std::sync::Arc::new(AsrBuffer::new());
        let mut exp = expander();
        exp.set_asr_buffer(buf);
        // __ASR_SUBMIT__ with empty buffer → empty (not preview "...")
        assert_eq!(exp.expand("__ASR_SUBMIT__").unwrap(), "");
    }
}
