//! Keeps every log record to one bounded line on every sink (stderr, OTLP, Sentry).
//!
//! An anyhow `{:?}` value carries its context chain over several lines and, when
//! backtraces are captured, a "Stack backtrace:" block of 40-odd frames whose
//! monomorphised names run to hundreds of bytes each: 8-16 KB per error, which broke
//! Loki queries (a 1,000-line result over its 4 MB gRPC cap) and filled kubectl with
//! multi-line noise. A record is rewritten as:
//!
//! - an anyhow report becomes its chain on one line, `outer: cause: root`, and its
//!   first backtrace becomes `error.origin`, the application frames only
//!   (`a::f <- b::g`), so the call path still shows on stderr when OTLP is down;
//! - any other text keeps its content, with line breaks and control characters
//!   escaped (`\n`, `\t`, `\u{1b}`) and ANSI colour codes dropped. Backslashes are
//!   not escaped, so a literal `\n` in the text reads like an escaped line break;
//! - keys, values and the body are capped on UTF-8 boundaries, and the whole record,
//!   the pairs appended after it included, fits [`RECORD_MAX`].
//!
//! Recognising a report is a heuristic over text, because `log` hands `e:?` values
//! over as Debug output only. It is conservative: a header counts only when the
//! indented block anyhow writes under it follows. Work is bounded (input size, lines,
//! symbol length, nesting depth) so that hostile text can neither panic this code nor
//! exhaust the stack: it also runs inside the panic hook, where a second panic aborts.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::sync::OnceLock;

/// Everything a record emits, in bytes: the body, every key and value, the pairs
/// appended after it and the ` key=` around each pair. 1,000 records then stay under
/// Loki's 4 MB gRPC message limit with room for labels and encoding.
pub(crate) const RECORD_MAX: usize = 3584;
const BODY_MAX: usize = 1024;
const ERROR_VALUE_MAX: usize = 1024;
const VALUE_MAX: usize = 512;
const KEY_MAX: usize = 64;
const ORIGIN_MAX: usize = 512;
const ORIGIN_FRAMES: usize = 8;
/// A pair that would get less room than this for its value is dropped and counted.
const MIN_SLICE: usize = 48;
/// Bounds on the work done for one value, whatever its size.
const RENDER_MAX: usize = 64 * 1024;
const LINES_MAX: usize = 4096;
const SYMBOL_MAX: usize = 2048;
const NESTING_MAX: usize = 8;

pub(crate) const SCOPE_MAX: usize = 128;
pub(crate) const ROOT_MAX: usize = 256;

const ORIGIN_KEY: &str = "error.origin";
const DROPPED_KEY: &str = "log.dropped";
/// A pair the panic hook passes its captured backtrace in: consumed into
/// `error.origin`, never emitted.
pub(crate) const BACKTRACE_KEY: &str = "backtrace";
const ORIGIN_SEPARATOR: &str = " <- ";

/// Keys this module and `signature` write. A record's own pair with one of these
/// names is dropped and counted, so each appears once.
const RESERVED: &[&str] = &[
    ORIGIN_KEY,
    DROPPED_KEY,
    "error.fingerprint",
    "error.scope",
    "error.root",
];

const fn pair(key: &str, value: usize) -> usize {
    key.len() + 2 + value
}
const SIGNATURE_RESERVE: usize =
    pair("error.fingerprint", 16) + pair("error.scope", SCOPE_MAX) + pair("error.root", ROOT_MAX);
const ORIGIN_RESERVE: usize = pair(ORIGIN_KEY, ORIGIN_MAX);
const DROPPED_RESERVE: usize = pair(DROPPED_KEY, 20);
/// The record's metadata sinks print or export: the target (stderr and OTLP), and the
/// file, line and module path (OTLP `code.*`), each made one line of at most
/// [`METADATA_MAX`]. Charged with the names the OTLP bridge gives them.
pub(crate) const METADATA_MAX: usize = 128;
const METADATA_RESERVE: usize = pair("target", METADATA_MAX)
    + pair("code.file.path", METADATA_MAX)
    + pair("code.line.number", 10)
    + pair("code.function.name", METADATA_MAX);
/// What the body and the record's own pairs share.
const SHARED: usize =
    RECORD_MAX - SIGNATURE_RESERVE - ORIGIN_RESERVE - DROPPED_RESERVE - METADATA_RESERVE;

/// Keys whose values are errors: served first and given the larger cap.
fn is_error_key(key: &str) -> bool {
    matches!(key, "e" | "err" | "error" | "reason" | "cause" | "why")
}

/// Crates whose frames are runtime, framework or std plumbing, used unless the
/// service names its own crates with [`application_crates`].
const PLUMBING_CRATES: &[&str] = &[
    "__rustc",
    "alloc",
    "anyhow",
    "async",
    "axum",
    "backtrace",
    "core",
    "deadpool",
    "diesel",
    "futures",
    "h2",
    "http",
    "hyper",
    "opentelemetry",
    "pin",
    "reqwest",
    "sentry",
    "serde",
    "std",
    "test",
    "tokio",
    "tonic",
    "tower",
    "tracing",
    "trail",
];

/// Primitive types, which head paths like `str::parse` without being crates.
const PRIMITIVES: &[&str] = &[
    "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "str", "u8", "u16",
    "u32", "u64", "u128", "usize",
];

static APPLICATION_CRATES: OnceLock<&'static [&'static str]> = OnceLock::new();

/// Names the crates whose frames make up `error.origin`: a name ending in `_` is a
/// prefix (`"queries_"` matches `queries_users`), any other must equal the crate.
/// Without this, every crate outside a built-in list of runtime and framework crates
/// counts. Call it before logging starts; it takes effect once and returns whether
/// this call set it.
pub fn application_crates(names: &'static [&'static str]) -> bool {
    APPLICATION_CRATES.set(names).is_ok()
}

fn is_application_crate(krate: &str) -> bool {
    let identifier = krate
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && krate.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !identifier || PRIMITIVES.contains(&krate) {
        return false;
    }
    match APPLICATION_CRATES.get() {
        Some(names) => listed(krate, names),
        // Built-in names also cover their family: `tokio` matches `tokio_util`.
        None => !PLUMBING_CRATES.iter().any(|n| {
            krate == *n
                || krate
                    .strip_prefix(n)
                    .is_some_and(|rest| rest.starts_with('_'))
        }),
    }
}

/// Whether `krate` is one of `names`, where a name ending in `_` is a prefix.
fn listed(krate: &str, names: &[&str]) -> bool {
    names.iter().any(|n| match n.strip_suffix('_') {
        Some(_) => krate.starts_with(n),
        None => krate == *n,
    })
}

fn is_application(path: &str) -> bool {
    path.split_once("::")
        .is_some_and(|(krate, _)| is_application_crate(krate))
}

/// One value rewritten to a single bounded line, plus the application frames of the
/// backtrace it carried, if any.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Line {
    pub text: String,
    pub origin: Option<String>,
}

/// Rewrites `text` to one line of at most `max` bytes.
pub(crate) fn line(text: &str, max: usize) -> Line {
    if !text.contains(char::is_control) {
        return Line {
            text: cap(text, max).into_owned(),
            origin: None,
        };
    }
    let lines: Vec<&str> = text.lines().take(LINES_MAX).collect();
    match report(&lines) {
        Some(report) => Line {
            text: cap(&report.chain, max).into_owned(),
            origin: origin(report.frames.iter().copied()),
        },
        None => Line {
            text: escaped(text, max),
            origin: None,
        },
    }
}

/// `text` on one line: control characters escaped, then capped.
pub(crate) fn single_line(text: &str, max: usize) -> Cow<'_, str> {
    if text.contains(char::is_control) {
        Cow::Owned(escaped(text, max))
    } else {
        cap(text, max)
    }
}

/// Skips an ANSI CSI sequence (`ESC [ params final`) at the start of `chars`,
/// returning whether there was one. Colour codes are presentation, not content. An
/// `ESC [` without a final byte within 32 characters is not one, and is kept.
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

fn escaped(text: &str, max: usize) -> String {
    // Reads at most RENDER_MAX bytes, and stops once the output passes `max`.
    let mut end = text.len().min(RENDER_MAX);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(max.min(end) + 16);
    let mut chars = text.get(..end).unwrap_or_default().chars().peekable();
    let mut complete = end == text.len();
    while let Some(c) = chars.next() {
        if out.len() > max {
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
    let total = if complete && out.len() <= max {
        out.len()
    } else {
        text.len()
    };
    cap_with_total(&out, max, total).into_owned()
}

struct Report<'a> {
    chain: String,
    frames: Vec<&'a str>,
}

fn indent_of(raw: &str) -> usize {
    raw.len() - raw.trim_start().len()
}

/// The next line that isn't blank, if it is indented deeper than `indent`.
fn deeper_follows<'a>(lines: &[&'a str], at: usize, indent: usize) -> Option<&'a str> {
    let next = lines.get(at + 1..)?.iter().find(|l| !l.trim().is_empty())?;
    (indent_of(next) > indent).then_some(*next)
}

/// Reads anyhow's Debug report: `msg`, then `Caused by:` with its causes indented
/// under it, then `Stack backtrace:` with its frames indented under it; any of them
/// nested, more deeply indented, inside a cause that interpolated another report.
/// None when no header is followed by its block, so ordinary text stays text.
fn report<'a>(lines: &[&'a str]) -> Option<Report<'a>> {
    let mut segments: Vec<String> = Vec::new();
    let mut frames: Vec<&'a str> = Vec::new();
    let mut backtrace: Option<usize> = None;
    let mut blocks = 0;
    let mut in_causes = false;
    let mut cause_open = false;
    let mut recognised = false;
    // One entry per open `Caused by:` header, innermost last: its indent, and whether
    // its causes are numbered (anyhow numbers them, from `0: `, only when there are
    // two or more, so a single cause starting `404: ` keeps its number).
    let mut headers: Vec<(usize, Option<bool>)> = Vec::new();

    for (at, raw) in lines.iter().enumerate() {
        let trimmed = raw.trim();
        let indent = indent_of(raw);
        if let Some(header) = backtrace {
            if trimmed.is_empty() {
                continue;
            }
            if indent > header {
                // A numbered frame, its `at` location, or another symbol inlined into
                // the same frame (std prints those unnumbered, aligned with the name).
                if !trimmed.starts_with("at ") && blocks == 1 {
                    frames.push(frame_symbol(trimmed).unwrap_or(trimmed));
                }
                continue;
            }
            backtrace = None;
        }
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "Stack backtrace:"
            && deeper_follows(lines, at, indent)
                .is_some_and(|next| frame_symbol(next.trim()).is_some())
        {
            backtrace = Some(indent);
            blocks += 1;
            recognised = true;
            continue;
        }
        if trimmed == "Caused by:" && deeper_follows(lines, at, indent).is_some() {
            headers.retain(|&(h, _)| h < indent);
            headers.push((indent, None));
            in_causes = true;
            cause_open = false;
            recognised = true;
            continue;
        }
        let clean = without_controls(trimmed);
        // Leaving a nested report's causes returns to the enclosing header's.
        while headers.len() > 1 && headers.last().is_some_and(|&(h, _)| indent <= h) {
            headers.pop();
        }
        if in_causes {
            // Ordinals only when anyhow wrote them: `0: ` followed, at the same
            // indent, by `1: `. A single cause may start with a number of its own.
            let numbering = match headers.last_mut() {
                Some((_, n @ None)) => *n.insert(
                    trimmed.starts_with("0: ")
                        && lines.get(at + 1..).is_some_and(|rest| {
                            rest.iter()
                                .any(|l| indent_of(l) == indent && l.trim().starts_with("1: "))
                        }),
                ),
                Some((_, Some(n))) => *n,
                None => false,
            };
            match numbered(trimmed).filter(|_| numbering) {
                Some(cause) => segments.push(without_controls(cause)),
                None if cause_open => append(&mut segments, &clean),
                None => segments.push(clean),
            }
            cause_open = true;
        } else if segments.is_empty() {
            segments.push(clean);
        } else {
            append(&mut segments, &clean);
        }
    }

    recognised.then(|| Report {
        chain: segments.join(": "),
        frames,
    })
}

/// The application frames of a bare backtrace (as `Backtrace` displays), for the
/// panic hook: numbered symbols and the unnumbered inlined ones under them.
pub(crate) fn backtrace_origin(backtrace: &str) -> Option<String> {
    origin(backtrace.lines().take(LINES_MAX).filter_map(|l| {
        let trimmed = l.trim();
        let inlined = !trimmed.is_empty() && !trimmed.starts_with("at ") && indent_of(l) > 0;
        frame_symbol(trimmed).or(inlined.then_some(trimmed))
    }))
}

fn append(segments: &mut [String], text: &str) {
    if let Some(last) = segments.last_mut() {
        last.push(' ');
        last.push_str(text);
    }
}

/// `"12: symbol"` or `"12: 0x55d4a1 - symbol"` -> `"symbol"`.
fn frame_symbol(trimmed: &str) -> Option<&str> {
    let rest = numbered(trimmed)?;
    let rest = match rest.strip_prefix("0x") {
        Some(address) => {
            let digits = address.bytes().take_while(u8::is_ascii_hexdigit).count();
            address.get(digits..)?.strip_prefix(" - ")?
        }
        None => rest,
    };
    let rest = rest.trim();
    (!rest.is_empty()).then_some(rest)
}

/// Strips anyhow's and std's `"N: "` ordinal.
fn numbered(trimmed: &str) -> Option<&str> {
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    trimmed.get(digits..)?.strip_prefix(": ")
}

fn without_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' if skip_ansi(&mut chars) => {}
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn origin<'a>(frames: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut out = String::new();
    let mut previous = String::new();
    let mut count = 0;
    for frame in frames {
        if count == ORIGIN_FRAMES {
            break;
        }
        let name = without_controls(&simplify(frame));
        if name == previous || !is_application(&name) {
            continue;
        }
        let separator = if out.is_empty() {
            0
        } else {
            ORIGIN_SEPARATOR.len()
        };
        if out.len() + separator + name.len() > ORIGIN_MAX {
            continue;
        }
        if !out.is_empty() {
            out.push_str(ORIGIN_SEPARATOR);
        }
        out.push_str(&name);
        previous = name;
        count += 1;
    }
    (!out.is_empty()).then_some(out)
}

/// A demangled symbol reduced to its path: generic arguments, turbofish, closure and
/// shim segments and legacy hashes dropped; `<T as Trait>::f` becomes `T::f`, or
/// `Trait::f` when only the trait belongs to the application.
pub(crate) fn simplify(symbol: &str) -> String {
    let path = unqualify(cut(symbol.trim(), SYMBOL_MAX));
    let mut out = String::with_capacity(path.len());
    let mut depth = 0usize;
    let mut chars = path.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '-' if chars.peek() == Some(&'>') => {
                chars.next();
            }
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split("::")
        .filter(|s| !s.is_empty() && !s.starts_with('{') && !is_legacy_hash(s))
        .collect::<Vec<_>>()
        .join("::")
}

fn strip_pointers(mut ty: &str) -> &str {
    for _ in 0..NESTING_MAX {
        let next = ["&mut ", "&", "*const ", "*mut ", "dyn ", "mut "]
            .iter()
            .find_map(|p| ty.strip_prefix(p));
        match next {
            Some(rest) => ty = rest.trim_start(),
            None => break,
        }
    }
    ty
}

/// The crate a type path starts with.
fn head_crate(ty: &str) -> &str {
    let ty = strip_pointers(ty);
    let end = ty.find(['<', ':', ' ']).unwrap_or(ty.len());
    ty.get(..end).unwrap_or_default()
}

/// Splits `A as B>rest` (the text after a leading `<`) into `A`, `B` and `rest`.
fn split_qualified(inner: &str) -> Option<(&str, Option<&str>, &str)> {
    let bytes = inner.as_bytes();
    let mut depth = 1usize;
    let mut as_at = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'-' if bytes.get(i + 1) == Some(&b'>') => i += 1,
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    let self_ty = inner.get(..as_at.unwrap_or(i))?;
                    let trait_ty = as_at.and_then(|a| inner.get(a + 4..i));
                    return Some((self_ty, trait_ty, inner.get(i + 1..)?));
                }
            }
            b' ' if depth == 1 && as_at.is_none() && inner.get(i..i + 4) == Some(" as ") => {
                as_at = Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// `<A as B>::rest` -> `A::rest` (or `B::rest` when only `B` is application code),
/// `<A>::rest` -> `A::rest`, through at most [`NESTING_MAX`] levels.
fn unqualify(symbol: &str) -> Cow<'_, str> {
    let mut head = symbol;
    let mut rests: Vec<&str> = Vec::new();
    for _ in 0..NESTING_MAX {
        let ty = strip_pointers(head);
        let Some((self_ty, trait_ty, rest)) = ty.strip_prefix('<').and_then(split_qualified) else {
            break;
        };
        rests.push(rest);
        // Only when both name a crate: a self type that is itself qualified (`<<A as
        // B>::C as D>`) is resolved by the next round instead.
        let crate_path = |ty: &str| strip_pointers(ty).contains("::") && !head_crate(ty).is_empty();
        head = match trait_ty {
            Some(t)
                if crate_path(self_ty)
                    && crate_path(t)
                    && !is_application_crate(head_crate(self_ty))
                    && is_application_crate(head_crate(t)) =>
            {
                t
            }
            _ => self_ty,
        };
    }
    if rests.is_empty() {
        return Cow::Borrowed(strip_pointers(head));
    }
    let mut out = String::from(strip_pointers(head));
    for rest in rests.iter().rev() {
        out.push_str(rest);
    }
    Cow::Owned(out)
}

fn is_legacy_hash(segment: &str) -> bool {
    segment.len() == 17
        && segment.starts_with('h')
        && segment.bytes().skip(1).all(|b| b.is_ascii_hexdigit())
}

/// `text` cut to at most `max` bytes on a char boundary, without a marker.
fn cut(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default()
}

/// Cuts `text` to at most `max` bytes on a char boundary, saying how long it was
/// when the marker fits.
pub(crate) fn cap(text: &str, max: usize) -> Cow<'_, str> {
    cap_with_total(text, max, text.len())
}

fn cap_with_total(text: &str, max: usize, total: usize) -> Cow<'_, str> {
    if text.len() <= max && total == text.len() {
        return Cow::Borrowed(text);
    }
    let marker = format!("…[{total} bytes in all]");
    if marker.len() > max {
        return Cow::Borrowed(cut(text, max));
    }
    let kept = cut(text, max - marker.len());
    Cow::Owned(format!("{kept}{marker}"))
}

/// A `fmt::Write` that keeps at most [`RENDER_MAX`] bytes and then stops the
/// formatter, so rendering a huge value costs bounded work and memory.
struct Bounded(String);

impl std::fmt::Write for Bounded {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        let room = RENDER_MAX.saturating_sub(self.0.len());
        if s.len() <= room {
            self.0.push_str(s);
            Ok(())
        } else {
            self.0.push_str(cut(s, room));
            Err(std::fmt::Error)
        }
    }
}

pub(crate) fn render(value: impl std::fmt::Display) -> String {
    let mut out = Bounded(String::new());
    let _ = write!(out, "{value}");
    out.0
}

fn decimal_len(mut n: u64) -> usize {
    let mut len = 1;
    while n >= 10 {
        n /= 10;
        len += 1;
    }
    len
}

/// The printed length of a value carried typed, or None for one that isn't a
/// bounded number or boolean.
fn typed_len(value: &log::kv::Value<'_>) -> Option<usize> {
    if let Some(n) = value.to_u64() {
        return Some(decimal_len(n));
    }
    if let Some(n) = value.to_i64() {
        return Some(decimal_len(n.unsigned_abs()) + usize::from(n < 0));
    }
    value.to_bool().map(|b| if b { 4 } else { 5 })
}

fn clean_key(key: &str) -> bool {
    key.len() <= KEY_MAX && !key.contains(char::is_control)
}

/// Whether the record already is one bounded line, so it can be dispatched as it
/// is: a literal body and pairs that are short clean strings or bounded numbers.
/// Reads no value through a formatter, so it never runs user code.
pub(crate) fn fits(record: &log::Record<'_>) -> bool {
    let Some(body) = record.args().as_str() else {
        return false;
    };
    if body.len() > BODY_MAX || body.contains(char::is_control) {
        return false;
    }
    let clean_meta = |m: Option<&str>| {
        m.is_none_or(|m| m.len() <= METADATA_MAX && !m.contains(char::is_control))
    };
    if !clean_meta(Some(record.target()))
        || !clean_meta(record.module_path())
        || !clean_meta(record.file())
    {
        return false;
    }
    struct Check {
        used: usize,
        ok: bool,
    }
    impl<'k> log::kv::VisitSource<'k> for Check {
        fn visit_pair(
            &mut self,
            key: log::kv::Key<'k>,
            value: log::kv::Value<'k>,
        ) -> Result<(), log::kv::Error> {
            let key = key.as_str();
            let max = if is_error_key(key) {
                ERROR_VALUE_MAX
            } else {
                VALUE_MAX
            };
            let len = match value.to_borrowed_str() {
                Some(s) if s.len() <= max && !s.contains(char::is_control) => Some(s.len()),
                Some(_) => None,
                None => typed_len(&value),
            };
            match len {
                Some(len) if clean_key(key) && !RESERVED.contains(&key) && key != BACKTRACE_KEY => {
                    self.used += pair(key, len);
                }
                _ => self.ok = false,
            }
            if self.ok {
                Ok(())
            } else {
                Err(log::kv::Error::msg("needs compaction"))
            }
        }
    }
    let mut check = Check {
        used: body.len(),
        ok: true,
    };
    let _ = record.key_values().visit(&mut check);
    check.ok && check.used <= SHARED
}

/// A record's body and pairs, rewritten to one bounded line.
pub(crate) struct Record<'a> {
    /// None: the record's own literal body is already clean.
    pub body: Option<String>,
    pairs: Vec<(Cow<'a, str>, Slot<'a>)>,
    origin: Option<String>,
    dropped: u64,
}

enum Slot<'a> {
    Kept(log::kv::Value<'a>),
    Owned(String),
}

impl<'a> Record<'a> {
    pub(crate) fn new(record: &'a log::Record<'a>) -> Self {
        let mut used = 0;
        let mut origin = None;

        let body = match record.args().as_str() {
            Some(s) if s.len() <= BODY_MAX && !s.contains(char::is_control) => {
                used += s.len();
                None
            }
            Some(s) => Some(own(s, BODY_MAX, &mut used, &mut origin)),
            None => {
                let rendered = render(record.args());
                if rendered.len() <= BODY_MAX && !rendered.contains(char::is_control) {
                    used += rendered.len();
                    Some(rendered)
                } else {
                    Some(own(&rendered, BODY_MAX, &mut used, &mut origin))
                }
            }
        };

        let mut collect = Collect { pairs: Vec::new() };
        let _ = record.key_values().visit(&mut collect);

        let mut dropped = 0u64;
        let mut slots: Vec<Option<(Cow<'a, str>, Slot<'a>)>> =
            collect.pairs.iter().map(|_| None).collect();
        // Error values are served first, so a long tail of other pairs can't starve
        // them; the output keeps the record's own order.
        for errors_pass in [true, false] {
            for (i, (key, value)) in collect.pairs.iter().enumerate() {
                let name = key.as_str();
                if is_error_key(name) != errors_pass {
                    continue;
                }
                // The panic hook's captured trace becomes the origin; on any other
                // record `backtrace` is an ordinary pair.
                if name == BACKTRACE_KEY && record.target() == crate::init::PANIC_TARGET {
                    if origin.is_none() {
                        origin = backtrace_origin(&render(value));
                    }
                    continue;
                }
                let key: Cow<'a, str> = match key.to_borrowed_str() {
                    Some(k) if clean_key(k) => Cow::Borrowed(k),
                    _ => Cow::Owned(single_line(key.as_str(), KEY_MAX).into_owned()),
                };
                // Checked on the key as emitted: cleaning can turn one into a reserved name.
                if RESERVED.contains(&key.as_ref()) {
                    dropped += 1;
                    continue;
                }
                let room = SHARED.saturating_sub(used + pair(&key, 0));
                if room < MIN_SLICE {
                    dropped += 1;
                    continue;
                }
                let max = room.min(if is_error_key(&key) {
                    ERROR_VALUE_MAX
                } else {
                    VALUE_MAX
                });
                // Numbers stay typed: measured, and dropped rather than cut when too long.
                let typed =
                    typed_len(value).or_else(|| value.to_f64().map(|_| render(value).len()));
                let slot = match (value.to_borrowed_str(), typed) {
                    (Some(s), _) if s.len() <= max && !s.contains(char::is_control) => {
                        used += s.len();
                        Slot::Kept(value.clone())
                    }
                    (Some(s), _) => Slot::Owned(own(s, max, &mut used, &mut origin)),
                    (None, Some(len)) if len <= max => {
                        used += len;
                        Slot::Kept(value.clone())
                    }
                    (None, Some(_)) => {
                        dropped += 1;
                        continue;
                    }
                    (None, None) => Slot::Owned(own(&render(value), max, &mut used, &mut origin)),
                };
                used += pair(&key, 0);
                if let Some(entry) = slots.get_mut(i) {
                    *entry = Some((key, slot));
                }
            }
        }

        Record {
            body,
            pairs: slots.into_iter().flatten().collect(),
            origin: origin.map(|o| cap(&o, ORIGIN_MAX).into_owned()),
            dropped,
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
            visitor.visit_pair(log::kv::Key::from_str(key), value)?;
        }
        if self.dropped > 0 {
            visitor.visit_pair(
                log::kv::Key::from_str(DROPPED_KEY),
                log::kv::Value::from(self.dropped),
            )?;
        }
        if let Some(origin) = &self.origin {
            visitor.visit_pair(
                log::kv::Key::from_str(ORIGIN_KEY),
                log::kv::Value::from(origin.as_str()),
            )?;
        }
        Ok(())
    }
}

fn own(text: &str, max: usize, used: &mut usize, origin: &mut Option<String>) -> String {
    let line = line(cut(text, RENDER_MAX), max);
    if origin.is_none() {
        *origin = line.origin;
    }
    *used += line.text.len();
    line.text
}

struct Collect<'a> {
    pairs: Vec<(log::kv::Key<'a>, log::kv::Value<'a>)>,
}

impl<'a> log::kv::VisitSource<'a> for Collect<'a> {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'a>,
        value: log::kv::Value<'a>,
    ) -> Result<(), log::kv::Error> {
        self.pairs.push((key, value));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A production record (2026-11-19, prices outage), frames cut to the shapes that matter.
    const OUTAGE: &str = "balances\n\nCaused by:\n    0: failed to get prices for BTC\n    1: can't calculate_price\n    2: couldn't find global balance snapshots\n\nStack backtrace:\n   0: <core::option::Option<queries_global_balances::GlobalBalancesForLiquid> as anyhow::Context<queries_global_balances::GlobalBalancesForLiquid, core::convert::Infallible>>::context::<&str>\n   1: bipa::features::liquid_quote_parts::assemble_price_bag::{closure#0}\n   2: bipa::features::liquid_quote_parts::calculate_price::{closure#0}\n   3: bipa::features::liquid_quotes::get_latest_reference_prices::<0>::{closure#0}\n   4: bipa::features::balances::get_inner::<0>::{closure#0}\n   5: <db_core::db::Db>::run_conn::<9, bipa::features::card::GetConfigOutcome, anyhow::Error, bipa::features::card::get_config::{closure#0}::{closure#0}, <db_core::db::Db>::read<bipa::features::card::GetConfigOutcome, anyhow::Error, bipa::features::card::get_config::{closure#0}::{closure#0}>::{closure#0}>::{closure#0}::{closure#0}\n   6: <opentelemetry::context::future_ext::WithContext<<db_core::db::Db>::run_conn<9, bipa::features::card::GetConfigOutcome>> as core::future::future::Future>::poll\n   7: <futures_util::future::future::map::Map<futures_util::future::try_future::into_future::IntoFuture<bipa::grpc::cards::get_config::{closure#0}>, futures_util::fns::MapOkFn<<tonic::response::Response<bipa_grpc_proto::mobile::GetCardConfigResponse>>::new>> as core::future::future::Future>::poll\n   8: <bipa::grpc::Server as bipa_grpc_proto::mobile::mobile_server::Mobile>::get_card_config::{closure#0}\n   9: <tokio::runtime::scheduler::multi_thread::worker::Context>::run_task\n  10: std::sys::backtrace::__rust_begin_short_backtrace::<<tokio::runtime::blocking::pool::Spawner>::spawn_thread::{closure#0}, ()>\n  11: <unknown>\n  12: clone";

    const TURBO: &str = "Failed renewing turbo plan, user_id: 311396\n\nCaused by:\n    should have configs\n\nStack backtrace:\n   0: <opentelemetry::context::future_ext::WithContext<T> as core::future::future::Future>::poll\n   1: bipa::crons::turbo_plan::renew_and_cancel::{{closure}}";

    #[test]
    fn outage_record_becomes_its_chain_and_application_frames() {
        let out = line(OUTAGE, ERROR_VALUE_MAX);
        assert_eq!(
            out.text,
            "balances: failed to get prices for BTC: can't calculate_price: couldn't find global balance snapshots"
        );
        assert_eq!(
            out.origin.as_deref(),
            Some(
                "bipa::features::liquid_quote_parts::assemble_price_bag <- bipa::features::liquid_quote_parts::calculate_price <- bipa::features::liquid_quotes::get_latest_reference_prices <- bipa::features::balances::get_inner <- db_core::db::Db::run_conn <- bipa::grpc::Server::get_card_config"
            )
        );
    }

    #[test]
    fn single_cause_and_legacy_closure_symbols() {
        let out = line(TURBO, ERROR_VALUE_MAX);
        assert_eq!(
            out.text,
            "Failed renewing turbo plan, user_id: 311396: should have configs"
        );
        assert_eq!(
            out.origin.as_deref(),
            Some("bipa::crons::turbo_plan::renew_and_cancel")
        );
    }

    // Real anyhow 1.0.104 reports, captured with RUST_LIB_BACKTRACE=1 from a release
    // build with line tables, so the frames carry `at file:line` lines. In the first, a
    // middle cause interpolates another report (`{e:?}`): its backtrace sits between
    // two causes with no blank line before the last one, so only indentation tells
    // them apart. In the second, a single cause starts with a number of its own.
    const NESTED: &str = include_str!("../testdata/anyhow-nested-line-tables.txt");
    const SINGLE_NUMBERED: &str = include_str!("../testdata/anyhow-single-numbered-cause.txt");

    #[test]
    fn captured_reports_keep_every_cause_and_find_their_frames() {
        let out = line(NESTED, ERROR_VALUE_MAX);
        assert_eq!(
            out.text,
            "outer: c0: wrapped: middle ctx: helper ctx: root cause: disk gone"
        );
        let lines: Vec<&str> = NESTED.lines().collect();
        let frames = report(&lines).unwrap().frames;
        assert_eq!(
            &frames[..4],
            ["msg<&str>", "helper", "middle", "nested_mid"]
        );
        assert!(frames.iter().all(|f| !f.starts_with("at ")), "{frames:?}");

        assert_eq!(
            line(SINGLE_NUMBERED, ERROR_VALUE_MAX).text,
            "request failed: 404: Not Found"
        );
    }

    // Built live, without backtraces (none are captured unless the environment asks),
    // so these pin the chain shapes whatever the test process's settings.
    #[test]
    fn live_reports_keep_their_chain() {
        use anyhow::{anyhow, Context};
        let inner = anyhow!("root cause").context("middle ctx");
        let nested = Err::<(), _>(std::io::Error::other("disk gone"))
            .context(format!("wrapped: {inner:?}"))
            .context("c0")
            .context("outer")
            .unwrap_err();
        assert_eq!(
            line(&format!("{nested:?}"), ERROR_VALUE_MAX).text,
            "outer: c0: wrapped: middle ctx: root cause: disk gone"
        );
        let multi = anyhow!("line one\nline two")
            .context("ctx a")
            .context("ctx b");
        assert_eq!(
            line(&format!("{multi:?}"), ERROR_VALUE_MAX).text,
            "ctx b: ctx a: line one line two"
        );
    }

    #[test]
    fn inlined_symbols_stay_in_the_backtrace() {
        // std prints a frame's further, inlined symbols unnumbered, aligned with its name.
        let text = "outer\n\nCaused by:\n    0: mid\n    1: root\n\nStack backtrace:\n   0: bipa::a::f\n             at ./src/a.rs:1:1\n      bipa::a::inlined_helper\n             at ./src/a.rs:9:5\n   1: bipa::b::g\n             at ./src/b.rs:2:2\n   2: tokio::runtime::x";
        let out = line(text, 1024);
        assert_eq!(out.text, "outer: mid: root");
        assert_eq!(
            out.origin.as_deref(),
            Some("bipa::a::f <- bipa::a::inlined_helper <- bipa::b::g")
        );
    }

    #[test]
    fn text_that_only_mentions_a_header_is_escaped_not_parsed() {
        let text = "why\nStack backtrace:\nnot a frame";
        assert_eq!(line(text, 256).text, "why\\nStack backtrace:\\nnot a frame");
        let text = "Caused by:\nsomething at the same indent";
        assert_eq!(
            line(text, 256).text,
            "Caused by:\\nsomething at the same indent"
        );
    }

    #[test]
    fn plain_text_keeps_its_content_with_control_characters_escaped() {
        assert_eq!(line("all on one line", 64).text, "all on one line");
        assert_eq!(line("tab\there\r\nnext", 64).text, "tab\\there\\r\\nnext");
        assert_eq!(line("nul\u{0}bel\u{7}", 64).text, "nul\\u{0}bel\\u{7}");
        // Colour codes are presentation: dropped, not escaped.
        assert_eq!(line("\u{1b}[1;32mready\u{1b}[0m", 64).text, "ready");
        assert_eq!(line("lone \u{1b} escape", 64).text, "lone \\u{1b} escape");
    }

    #[test]
    fn full_format_frames_with_addresses_and_locations() {
        let text = "boom\n\nCaused by:\n    root\n\nStack backtrace:\n   0: 0x55d4a1b2 - tokio::runtime::run\n   1: 0x55d4a1b3 - bipa::x::f\n             at ./src/x.rs:1:1\n   2: 0x55d4a1b4 - <&std::thread::Builder as core::ops::FnOnce>::call_once\n   3: 0x55d4a1b5 - <core::cell::RefCell<T> as bipa::Ext>::g";
        let out = line(text, 1024);
        assert_eq!(out.text, "boom: root");
        assert_eq!(out.origin.as_deref(), Some("bipa::x::f <- bipa::Ext::g"));
    }

    #[test]
    fn caps_hold_their_limit_and_cut_on_char_boundaries() {
        let text = "ação ".repeat(100);
        let out = cap(&text, 64);
        assert!(out.len() <= 64, "{}", out.len());
        assert!(out.ends_with(&format!("…[{} bytes in all]", text.len())));
        assert_eq!(cap("short", 64), "short");
        for max in 0..24 {
            assert!(cap(&text, max).len() <= max, "{max}");
            assert!(
                single_line("a\nb\nc\nd\ne\nf\ng\nh\ni", max).len() <= max,
                "{max}"
            );
        }
    }

    #[test]
    fn symbols_reduce_to_their_paths() {
        for (symbol, path) in [
            ("<bipa::grpc::Server as bipa_grpc_proto::mobile::mobile_server::Mobile>::get_card_config::{closure#0}", "bipa::grpc::Server::get_card_config"),
            ("<db_core::db::Db>::run_conn::<9, (), anyhow::Error, X<Y>>::{closure#0}", "db_core::db::Db::run_conn"),
            ("bipa::features::liquid_quotes::get_latest_reference_prices::<0>::{closure#0}", "bipa::features::liquid_quotes::get_latest_reference_prices"),
            ("bipa::crons::turbo_plan::renew_and_cancel::{{closure}}", "bipa::crons::turbo_plan::renew_and_cancel"),
            ("bipa::f::h0123456789abcdef", "bipa::f"),
            ("<std::thread::Builder>::spawn_unchecked_::<fn() -> u8, u8>::{closure#1}", "std::thread::Builder::spawn_unchecked_"),
            ("<<A<_> as B<C>>::call::Svc<D> as E>::call::{closure#0}", "A::call::Svc::call"),
            ("<&mut bipa::x::Y as core::fmt::Debug>::fmt", "bipa::x::Y::fmt"),
            ("<core::cell::RefCell<T> as bipa::Ext>::g", "bipa::Ext::g"),
        ] {
            assert_eq!(simplify(symbol), path, "{symbol}");
        }
    }

    #[test]
    fn plumbing_and_bare_symbols_are_not_application_frames() {
        for name in [
            "tokio::runtime::run",
            "futures_util::f",
            "opentelemetry_sdk::x",
            "core::f",
            "clone",
            "<unknown>",
            "trail::init::f",
            "__rustc::rust_begin_unwind",
            "str::parse",
            "u64::from_str",
            "[u8]::to_vec",
            "(A, B)::fmt",
            "fn()  core::result::Result::call_once",
            "serde_json::de::from_str",
            "test::run_test",
        ] {
            assert!(!is_application(name), "{name}");
        }
        for name in [
            "bipa::f",
            "db_core::db::Db::run_conn",
            "httparse_like::f",
            "corex::f",
        ] {
            assert!(is_application(name), "{name}");
        }
    }

    #[test]
    fn an_application_list_matches_exact_names_and_underscore_prefixes() {
        let names = ["bipa", "db_core", "queries_"];
        for krate in ["bipa", "db_core", "queries_users", "queries_"] {
            assert!(listed(krate, &names), "{krate}");
        }
        for krate in ["bipa_grpc_proto", "db", "queries", "db_core_x"] {
            assert!(!listed(krate, &names), "{krate}");
        }
    }

    #[test]
    fn origin_keeps_at_most_eight_frames_and_skips_oversized_ones() {
        let frames = (0..40)
            .map(|i| format!("{i:4}: bipa::module_{i}::function_{i}::{{closure#0}}"))
            .collect::<Vec<_>>()
            .join("\n");
        let origin = backtrace_origin(&frames).unwrap();
        assert_eq!(origin.split(ORIGIN_SEPARATOR).count(), ORIGIN_FRAMES);
        assert!(origin.len() <= ORIGIN_MAX);

        let repeated = "   0: bipa::a::f\n   1: bipa::a::f::{closure#0}\n   2: bipa::b::g";
        assert_eq!(
            backtrace_origin(repeated).as_deref(),
            Some("bipa::a::f <- bipa::b::g")
        );
        assert_eq!(backtrace_origin("   0: tokio::x\n   1: std::y"), None);

        let long = format!("   0: bipa::{}::f\n   1: bipa::b::g", "x".repeat(600));
        assert_eq!(backtrace_origin(&long).as_deref(), Some("bipa::b::g"));
    }

    #[test]
    fn hostile_input_never_panics_and_stays_shallow() {
        let deep = format!(
            "   0: {}bipa::f{}",
            "<".repeat(100_000),
            ">".repeat(100_000)
        );
        let qualified = format!("   0: {}bipa::f as T>::g", "<".repeat(50_000));
        for text in [
            "",
            "\n\n\n",
            "Stack backtrace:",
            "Stack backtrace:\n   0: ",
            "Caused by:",
            "   99999999999999999999999: <<<<",
            ">>>> as <<",
            "0: <a as",
            "é\nação\n\u{0}\u{7f}",
            "<",
            "<>::",
            "<a -> b>::c",
            "   0: 0x",
            "   0: 0xzz - f",
            deep.as_str(),
            qualified.as_str(),
        ] {
            let _ = line(text, 8);
            let _ = backtrace_origin(text);
            let _ = simplify(text);
            let _ = cap(text, 1);
            let _ = single_line(text, 0);
        }
    }

    fn rendered_size(record: &log::Record<'_>) -> usize {
        struct Size(usize);
        impl<'k> log::kv::VisitSource<'k> for Size {
            fn visit_pair(
                &mut self,
                key: log::kv::Key<'k>,
                value: log::kv::Value<'k>,
            ) -> Result<(), log::kv::Error> {
                self.0 += pair(key.as_str(), value.to_string().len());
                Ok(())
            }
        }
        let mut size = Size(record.args().to_string().len());
        let _ = record.key_values().visit(&mut size);
        size.0
    }

    /// The record as the logger hands it to the sinks: compacted, signature appended.
    fn emitted(record: &log::Record<'_>, f: impl FnOnce(&log::Record<'_>)) {
        let mut sig = crate::signature::compute(record);
        sig.scope = single_line(&sig.scope, SCOPE_MAX).into_owned();
        sig.root = single_line(&sig.root, ROOT_MAX).into_owned();
        let compact = Record::new(record);
        let kvs = crate::signature::WithSignature::new(&compact, &sig);
        match compact.body.as_deref() {
            Some(body) => f(&record
                .to_builder()
                .args(format_args!("{body}"))
                .key_values(&kvs)
                .build()),
            None => f(&record.to_builder().key_values(&kvs).build()),
        }
    }

    fn pairs_of(record: &log::Record<'_>) -> Vec<(String, String)> {
        struct All(Vec<(String, String)>);
        impl<'k> log::kv::VisitSource<'k> for All {
            fn visit_pair(
                &mut self,
                key: log::kv::Key<'k>,
                value: log::kv::Value<'k>,
            ) -> Result<(), log::kv::Error> {
                self.0.push((key.as_str().to_owned(), value.to_string()));
                Ok(())
            }
        }
        let mut all = All(Vec::new());
        let _ = record.key_values().visit(&mut all);
        all.0
    }

    #[test]
    fn a_record_with_a_backtrace_gets_one_line_values_and_an_origin() {
        let kvs = [
            ("user_id", log::kv::Value::from(42)),
            ("e", log::kv::Value::from(OUTAGE)),
        ];
        let record = log::Record::builder()
            .level(log::Level::Error)
            .module_path(Some("bipa::grpc::balances"))
            .args(format_args!("balances failed"))
            .key_values(&kvs)
            .build();
        emitted(&record, |out| {
            let pairs = pairs_of(out);
            let e = &pairs.iter().find(|(k, _)| k == "e").unwrap().1;
            assert!(e.starts_with("balances: failed to get prices for BTC"));
            let origin = &pairs.iter().find(|(k, _)| k == ORIGIN_KEY).unwrap().1;
            assert!(origin.starts_with("bipa::features::liquid_quote_parts::assemble_price_bag"));
            assert_eq!(pairs.iter().find(|(k, _)| k == "user_id").unwrap().1, "42");
            assert!(pairs
                .iter()
                .all(|(k, v)| !k.contains('\n') && !v.contains('\n')));
            assert!(rendered_size(out) <= RECORD_MAX);
        });
    }

    #[test]
    fn a_literal_multi_line_body_and_its_root_are_escaped() {
        let record = log::Record::builder()
            .level(log::Level::Error)
            .module_path(Some("bipa::x"))
            .args(format_args!("first\nsecond\tthird"))
            .build();
        assert!(!fits(&record));
        emitted(&record, |out| {
            assert_eq!(out.args().to_string(), "first\\nsecond\\tthird");
            let pairs = pairs_of(out);
            let root = &pairs.iter().find(|(k, _)| k == "error.root").unwrap().1;
            assert!(!root.contains(char::is_control), "{root}");
        });
    }

    #[test]
    fn short_clean_records_pass_through_untouched() {
        let kvs = [
            ("user_id", log::kv::Value::from(42)),
            ("state", log::kv::Value::from("open")),
        ];
        let record = log::Record::builder()
            .level(log::Level::Info)
            .args(format_args!("opened"))
            .key_values(&kvs)
            .build();
        assert!(fits(&record));

        let dirty = [("state", log::kv::Value::from("a\u{1b}[31m"))];
        let record = log::Record::builder()
            .args(format_args!("opened"))
            .key_values(&dirty)
            .build();
        assert!(!fits(&record));
        emitted(&record, |out| {
            assert_eq!(pairs_of(out)[0].1, "a");
        });
    }

    #[test]
    fn floats_stay_typed() {
        let kvs = [
            ("rate", log::kv::Value::from(0.25f64)),
            ("huge", log::kv::Value::from(f64::MAX)),
            ("note", log::kv::Value::from("x\ny")),
        ];
        let record = log::Record::builder()
            .args(format_args!("rated"))
            .key_values(&kvs)
            .build();
        let compact = Record::new(&record);
        for (key, value) in [("rate", 0.25), ("huge", f64::MAX)] {
            let (_, slot) = compact.pairs.iter().find(|(k, _)| k == key).unwrap();
            assert!(
                matches!(slot, Slot::Kept(v) if v.to_f64() == Some(value)),
                "{key}"
            );
        }
        assert_eq!(compact.dropped, 0);
    }

    #[test]
    fn reserved_keys_are_dropped_and_counted_as_a_number() {
        let kvs = [
            ("error.origin", log::kv::Value::from("spoofed")),
            ("error.root", log::kv::Value::from("spoofed")),
            ("e", log::kv::Value::from(TURBO)),
        ];
        let record = log::Record::builder()
            .level(log::Level::Error)
            .args(format_args!("renew failed"))
            .key_values(&kvs)
            .build();
        emitted(&record, |out| {
            let pairs = pairs_of(out);
            assert_eq!(pairs.iter().filter(|(k, _)| k == ORIGIN_KEY).count(), 1);
            assert_eq!(pairs.iter().filter(|(k, _)| k == "error.root").count(), 1);
            assert!(pairs.iter().all(|(_, v)| v != "spoofed"));
            assert_eq!(pairs.iter().find(|(k, _)| k == DROPPED_KEY).unwrap().1, "2");
        });
        let compact = Record::new(&record);
        assert_eq!(compact.dropped, 2);
    }

    #[test]
    fn any_record_stays_within_the_budget() {
        let big = "x".repeat(10_000);
        let keys: Vec<String> = (0..300).map(|i| format!("k{i:03}")).collect();
        let mut kvs: Vec<(&str, log::kv::Value<'_>)> = keys
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let value = match i % 3 {
                    0 => log::kv::Value::from(u64::MAX),
                    1 => log::kv::Value::from(big.as_str()),
                    _ => log::kv::Value::from(i64::MIN),
                };
                (k.as_str(), value)
            })
            .collect();
        kvs.push(("e", log::kv::Value::from(OUTAGE)));
        let long_key = "k".repeat(500);
        kvs.push((long_key.as_str(), log::kv::Value::from(1)));
        emitted(
            &log::Record::builder()
                .level(log::Level::Error)
                .module_path(Some(&big[..3000]))
                .args(format_args!("{big}\n{OUTAGE}"))
                .key_values(&kvs)
                .build(),
            |out| {
                let size = rendered_size(out);
                assert!(size <= RECORD_MAX, "{size}");
                let pairs = pairs_of(out);
                let e = pairs.iter().position(|(k, _)| k == "e").unwrap();
                assert!(pairs[e]
                    .1
                    .starts_with("balances: failed to get prices for BTC"));
                assert!(pairs.iter().any(|(k, _)| k == DROPPED_KEY));
                assert!(pairs
                    .iter()
                    .all(|(k, v)| k.len() <= KEY_MAX && !v.contains('\n')));
            },
        );
    }

    #[test]
    fn generated_origins_and_cleaned_keys_hold_the_contract() {
        let dirty = "outer\n\nCaused by:\n    root\n\nStack backtrace:\n   0: bipa::read\tfile\u{1b}[31m\n   1: bipa::b::g";
        let origin = line(dirty, 1024).origin.unwrap();
        assert!(!origin.contains(char::is_control), "{origin:?}");

        let spoof = [
            (
                "\u{1b}[31merror.origin\u{1b}[0m",
                log::kv::Value::from("spoofed"),
            ),
            ("e", log::kv::Value::from(TURBO)),
        ];
        let record = log::Record::builder()
            .level(log::Level::Error)
            .args(format_args!("renew failed"))
            .key_values(&spoof)
            .build();
        emitted(&record, |out| {
            let pairs = pairs_of(out);
            assert_eq!(pairs.iter().filter(|(k, _)| k == ORIGIN_KEY).count(), 1);
            assert!(pairs.iter().all(|(_, v)| v != "spoofed"));
        });
    }

    #[test]
    fn backtrace_is_an_ordinary_pair_outside_the_panic_hook() {
        let kvs = [(
            "backtrace",
            log::kv::Value::from("disabled: diagnostic context"),
        )];
        let record = log::Record::builder()
            .target("bipa::x")
            .args(format_args!("started"))
            .key_values(&kvs)
            .build();
        let compact = Record::new(&record);
        assert!(compact.pairs.iter().any(|(k, _)| k == "backtrace"));
    }

    #[test]
    fn colour_codes_never_crowd_out_the_text() {
        let text = format!("{}READY", "\u{1b}[31m".repeat(300));
        assert_eq!(line(&text, 64).text, "READY");
        // Without a final byte it isn't a colour code: kept, escaped.
        let unterminated = format!("x \u{1b}[{} tail", "1234567890".repeat(4));
        assert_eq!(
            line(&unterminated, 256).text,
            format!("x \\u{{1b}}[{} tail", "1234567890".repeat(4))
        );
    }

    #[test]
    fn inlined_symbols_count_in_a_bare_backtrace() {
        let bare = "   0: std::backtrace::Backtrace::force_capture\n   1: tokio::runtime::task::poll\n      bipa::jobs::run\n             at ./src/jobs.rs:3:3\n   2: std::rt::lang_start";
        assert_eq!(backtrace_origin(bare).as_deref(), Some("bipa::jobs::run"));
    }

    #[test]
    fn a_single_cause_numbered_zero_keeps_its_number() {
        use anyhow::{anyhow, Context};
        let e = Err::<(), _>(anyhow!("0: Not Found"))
            .context("request failed")
            .unwrap_err();
        assert_eq!(
            line(&format!("{e:?}"), ERROR_VALUE_MAX).text,
            "request failed: 0: Not Found"
        );
    }
}
