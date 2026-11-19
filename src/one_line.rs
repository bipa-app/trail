//! Keeps each log record on one line, on stderr and in OTLP alike: line breaks and
//! other control characters in the body and in pair values are escaped (`\n`, `\t`,
//! `\u{0}`), ANSI colour codes are dropped, and each is capped at [`MAX`] bytes on a
//! UTF-8 boundary, ending in `…[N bytes in all]`.
//!
//! An anyhow `e:?` value renders as a multi-line report; it stays whole, as escaped
//! text. Its size is kept down by not capturing error backtraces
//! (`RUST_LIB_BACKTRACE=0`), and the cap bounds whatever still gets through.

use std::borrow::Cow;
use std::fmt::Write as _;

/// The cap on the body and on each pair value, in bytes.
const MAX: usize = 2048;
/// A value is rendered into at most this many bytes, whatever its size.
const RENDER_MAX: usize = 64 * 1024;

fn needs_rewrite(text: &str) -> bool {
    text.len() > MAX || text.contains(char::is_control)
}

/// `text` on one line of at most [`MAX`] bytes.
pub(crate) fn clean(text: &str) -> Cow<'_, str> {
    if !needs_rewrite(text) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len().min(MAX) + 16);
    let mut chars = text.chars().peekable();
    let mut complete = true;
    while let Some(c) = chars.next() {
        if out.len() > MAX {
            complete = false;
            break;
        }
        match c {
            '\u{1b}' if skip_ansi(&mut chars) => {}
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    if complete && out.len() <= MAX {
        return Cow::Owned(out);
    }
    let marker = format!("…[{} bytes in all]", text.len());
    let mut end = MAX - marker.len();
    while !out.is_char_boundary(end) {
        end -= 1;
    }
    out.truncate(end);
    out.push_str(&marker);
    Cow::Owned(out)
}

/// Skips an ANSI CSI sequence (`ESC [ params final`) at the start of `chars`. One
/// without a final byte within 32 characters isn't one, and is kept (escaped).
fn skip_ansi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    if chars.peek() != Some(&'[') {
        return false;
    }
    let mut ahead = chars.clone();
    ahead.next();
    let Some(len) = ahead
        .take(32)
        .position(|c| ('\u{40}'..='\u{7e}').contains(&c))
    else {
        return false;
    };
    for _ in 0..len + 2 {
        chars.next();
    }
    true
}

/// Keeps at most [`RENDER_MAX`] bytes and then stops the formatter, so rendering a
/// huge value costs bounded work and memory.
struct Bounded(String);

impl std::fmt::Write for Bounded {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let room = RENDER_MAX.saturating_sub(self.0.len());
        if s.len() <= room {
            self.0.push_str(s);
            return Ok(());
        }
        let mut end = room;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        self.0.push_str(&s[..end]);
        Err(std::fmt::Error)
    }
}

fn render(value: impl std::fmt::Display) -> String {
    let mut out = Bounded(String::new());
    let _ = write!(out, "{value}");
    out.0
}

fn is_typed(value: &log::kv::Value<'_>) -> bool {
    value.to_u64().is_some()
        || value.to_i64().is_some()
        || value.to_f64().is_some()
        || value.to_bool().is_some()
}

/// Whether the record already is one short line, so it can be dispatched as it is:
/// a short literal body, and values that are short clean strings or numbers. Reads no
/// value through a formatter.
pub(crate) fn fits(record: &log::Record<'_>) -> bool {
    struct Check(bool);
    impl<'k> log::kv::VisitSource<'k> for Check {
        fn visit_pair(
            &mut self,
            _: log::kv::Key<'k>,
            value: log::kv::Value<'k>,
        ) -> Result<(), log::kv::Error> {
            self.0 = match value.to_borrowed_str() {
                Some(s) => !needs_rewrite(s),
                None => is_typed(&value),
            };
            if self.0 {
                Ok(())
            } else {
                Err(log::kv::Error::msg("needs rewriting"))
            }
        }
    }
    let Some(body) = record.args().as_str() else {
        return false;
    };
    let mut check = Check(true);
    let _ = record.key_values().visit(&mut check);
    check.0 && !needs_rewrite(body)
}

/// A record's body and pairs, each on one line of at most [`MAX`] bytes.
pub(crate) struct Record<'a> {
    /// None: the record's own literal body is already clean.
    pub body: Option<String>,
    pairs: Vec<(log::kv::Key<'a>, Slot<'a>)>,
}

enum Slot<'a> {
    Kept(log::kv::Value<'a>),
    Owned(String),
}

impl<'a> Record<'a> {
    pub(crate) fn new(record: &'a log::Record<'a>) -> Self {
        let body = match record.args().as_str() {
            Some(s) if !needs_rewrite(s) => None,
            Some(s) => Some(clean(s).into_owned()),
            None => {
                let rendered = render(record.args());
                Some(match clean(&rendered) {
                    Cow::Borrowed(_) => rendered,
                    Cow::Owned(cleaned) => cleaned,
                })
            }
        };

        struct Collect<'a>(Vec<(log::kv::Key<'a>, Slot<'a>)>);
        impl<'a> log::kv::VisitSource<'a> for Collect<'a> {
            fn visit_pair(
                &mut self,
                key: log::kv::Key<'a>,
                value: log::kv::Value<'a>,
            ) -> Result<(), log::kv::Error> {
                let slot = match value.to_borrowed_str() {
                    Some(s) if !needs_rewrite(s) => Slot::Kept(value),
                    Some(s) => Slot::Owned(clean(s).into_owned()),
                    None if is_typed(&value) => Slot::Kept(value),
                    None => {
                        let rendered = render(&value);
                        Slot::Owned(match clean(&rendered) {
                            Cow::Borrowed(_) => rendered,
                            Cow::Owned(cleaned) => cleaned,
                        })
                    }
                };
                self.0.push((key, slot));
                Ok(())
            }
        }
        let mut collect = Collect(Vec::new());
        let _ = record.key_values().visit(&mut collect);
        Record {
            body,
            pairs: collect.0,
        }
    }
}

impl log::kv::Source for Record<'_> {
    fn visit<'k>(
        &'k self,
        visitor: &mut dyn log::kv::VisitSource<'k>,
    ) -> Result<(), log::kv::Error> {
        for (key, slot) in &self.pairs {
            let value = match slot {
                Slot::Kept(v) => v.clone(),
                Slot::Owned(s) => log::kv::Value::from(s.as_str()),
            };
            visitor.visit_pair(key.clone(), value)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // An anyhow report as `e:?` renders it with error backtraces off.
    const REPORT: &str = "balances\n\nCaused by:\n    0: failed to get prices for BTC\n    1: couldn't find global balance snapshots";

    #[test]
    fn a_report_stays_whole_on_one_line() {
        assert_eq!(
            clean(REPORT),
            "balances\\n\\nCaused by:\\n    0: failed to get prices for BTC\\n    1: couldn't find global balance snapshots"
        );
    }

    #[test]
    fn control_characters_are_escaped_and_colour_codes_dropped() {
        assert_eq!(clean("tab\there\r\nnul\u{0}"), "tab\\there\\r\\nnul\\u{0}");
        assert_eq!(clean("\u{1b}[1;32mready\u{1b}[0m"), "ready");
        let unterminated = format!("x \u{1b}[{} tail", "1234567890".repeat(4));
        assert_eq!(
            clean(&unterminated),
            format!("x \\u{{1b}}[{} tail", "1234567890".repeat(4))
        );
        assert!(matches!(clean("already clean"), Cow::Borrowed(_)));
    }

    #[test]
    fn long_text_is_cut_on_a_char_boundary_with_its_size() {
        let text = "ação\n".repeat(2000);
        let out = clean(&text);
        assert!(out.len() <= MAX, "{}", out.len());
        assert!(out.ends_with(&format!("…[{} bytes in all]", text.len())));
        assert!(!out.contains('\n'));
    }

    struct Report;
    impl std::fmt::Debug for Report {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(REPORT)
        }
    }

    #[test]
    fn a_record_keeps_numbers_typed_and_rewrites_the_rest() {
        let report = Report;
        let kvs = [
            ("user_id", log::kv::Value::from(42)),
            ("rate", log::kv::Value::from(0.5)),
            ("state", log::kv::Value::from("open")),
            ("e", log::kv::Value::from_debug(&report)),
        ];
        let record = log::Record::builder()
            .args(format_args!("balances failed"))
            .key_values(&kvs)
            .build();
        assert!(!fits(&record));
        let compact = Record::new(&record);
        assert!(compact.body.is_none());
        let slot = |key: &str| {
            &compact
                .pairs
                .iter()
                .find(|(k, _)| k.as_str() == key)
                .unwrap()
                .1
        };
        assert!(matches!(slot("user_id"), Slot::Kept(v) if v.to_i64() == Some(42)));
        assert!(matches!(slot("rate"), Slot::Kept(v) if v.to_f64() == Some(0.5)));
        assert!(matches!(slot("state"), Slot::Kept(_)));
        assert!(
            matches!(slot("e"), Slot::Owned(s) if !s.contains('\n') && s.starts_with("balances\\n"))
        );
    }

    #[test]
    fn short_clean_records_pass_through() {
        let kvs = [
            ("user_id", log::kv::Value::from(42)),
            ("state", log::kv::Value::from("open")),
        ];
        let record = log::Record::builder()
            .args(format_args!("opened"))
            .key_values(&kvs)
            .build();
        assert!(fits(&record));
        let record = log::Record::builder()
            .args(format_args!("first\nsecond"))
            .build();
        assert!(!fits(&record));
        assert_eq!(Record::new(&record).body.as_deref(), Some("first\\nsecond"));
    }
}
