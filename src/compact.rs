//! Keeps every log record to one bounded line on every sink (stderr, OTLP, Sentry).
//!
//! An anyhow `{:?}` value carries its context chain over several lines and, when
//! backtraces are captured, a "Stack backtrace:" block of 40-odd frames whose
//! monomorphised names run to hundreds of bytes each: 8-16 KB per error, which broke
//! Loki queries (a 1,000-line result over its 4 MB gRPC cap) and filled kubectl with
//! multi-line noise. Here each value is rendered once and rewritten as:
//!
//! - its chain on one line, `outer: cause: root`;
//! - its backtrace reduced to the application frames, kept apart as `error.origin`
//!   (`a::f <- b::g <- ...`), so the call path survives on stderr when OTLP is down;
//! - a cap per value and per record, cut on a UTF-8 boundary with a marker.
//!
//! Parsing is best effort and must never panic: this runs inside the panic hook,
//! where a second panic aborts. Text that isn't anyhow-shaped only has its line
//! breaks and control characters replaced.

use std::borrow::Cow;

/// Per-record budget in bytes, keys included: 1,000 records stay under Loki's 4 MB
/// gRPC message limit with room for labels and the signature kvs appended later.
const RECORD_MAX: usize = 3584;
const BODY_MAX: usize = 1024;
const ERROR_VALUE_MAX: usize = 1024;
const VALUE_MAX: usize = 512;
const ORIGIN_MAX: usize = 512;
const ORIGIN_FRAMES: usize = 8;
/// A value that would get less room than this is dropped and counted, not cut.
const MIN_SLICE: usize = 48;

const ORIGIN_KEY: &str = "error.origin";
const DROPPED_KEY: &str = "log.dropped";
const ORIGIN_SEPARATOR: &str = " <- ";

/// Keys whose values are errors: given the larger cap, as `signature` reads them.
fn is_error_key(key: &str) -> bool {
    matches!(key, "e" | "err" | "error" | "reason" | "cause" | "why")
}

/// Crates whose frames are runtime, framework or std plumbing, never the place an
/// application error is worth locating. Matches the crate name or `name_*`.
const PLUMBING_CRATES: &[&str] = &[
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
    "std",
    "tokio",
    "tonic",
    "tower",
    "tracing",
    "trail",
];

/// One value rewritten to a single bounded line, plus the application frames of a
/// backtrace it carried, if any.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Line {
    pub text: String,
    pub origin: Option<String>,
}

/// Rewrites `text` to one line of at most `max` bytes, splitting off any backtrace.
pub(crate) fn line(text: &str, max: usize) -> Line {
    if !text.contains(char::is_control) {
        return Line {
            text: cap(text, max).into_owned(),
            origin: None,
        };
    }

    let mut segments: Vec<String> = Vec::new();
    let mut frames: Vec<&str> = Vec::new();
    let mut saw_backtrace = false;
    let mut in_backtrace = false;
    let mut in_causes = false;
    let mut cause_open = false;

    for raw in text.lines() {
        let trimmed = raw.trim();
        if in_backtrace {
            if let Some(symbol) = frame_symbol(trimmed) {
                if !saw_backtrace {
                    frames.push(symbol);
                }
                continue;
            }
            if raw.starts_with(char::is_whitespace) && trimmed.starts_with("at ") {
                continue;
            }
            // std prints frames without blank lines; anyhow separates sections with
            // one, so a blank line ends the block before a numbered cause can pass for
            // a frame.
            if trimmed.is_empty() {
                in_backtrace = false;
                saw_backtrace = true;
                continue;
            }
            in_backtrace = false;
            saw_backtrace = true;
        }
        if trimmed == "Stack backtrace:" {
            in_backtrace = true;
            continue;
        }
        if trimmed == "Caused by:" {
            in_causes = true;
            cause_open = false;
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        let clean = without_controls(trimmed);
        if in_causes {
            match numbered(trimmed) {
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

    let joined = segments.join(": ");
    Line {
        text: cap(&joined, max).into_owned(),
        origin: origin(frames.iter().copied()),
    }
}

/// The application frames of a bare backtrace (no header, as `Backtrace` displays),
/// for the panic hook.
pub(crate) fn backtrace_origin(backtrace: &str) -> Option<String> {
    origin(backtrace.lines().filter_map(|l| frame_symbol(l.trim())))
}

fn append(segments: &mut [String], text: &str) {
    if let Some(last) = segments.last_mut() {
        last.push(' ');
        last.push_str(text);
    }
}

/// `"12: symbol"` -> `"symbol"`.
fn frame_symbol(trimmed: &str) -> Option<&str> {
    let rest = numbered(trimmed)?;
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
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn origin<'a>(frames: impl Iterator<Item = &'a str>) -> Option<String> {
    let mut out = String::new();
    let mut previous = String::new();
    let mut count = 0;
    for frame in frames {
        let name = simplify(frame);
        if name == previous || !is_application(&name) {
            continue;
        }
        let added = if out.is_empty() {
            0
        } else {
            ORIGIN_SEPARATOR.len()
        } + name.len();
        if count == ORIGIN_FRAMES || out.len() + added > ORIGIN_MAX {
            break;
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

fn is_application(name: &str) -> bool {
    let Some((krate, _)) = name.split_once("::") else {
        return false;
    };
    !PLUMBING_CRATES.iter().any(|p| {
        krate == *p
            || krate
                .strip_prefix(p)
                .is_some_and(|rest| rest.starts_with('_'))
    })
}

/// A demangled symbol reduced to its path: generic arguments, turbofish, closure and
/// shim segments and legacy hashes dropped; `<T as Trait>::f` becomes `T::f`.
pub(crate) fn simplify(symbol: &str) -> String {
    let path = qualified_self(symbol.trim());
    let mut out = String::with_capacity(path.len());
    let mut depth = 0usize;
    let mut chars = path.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        match c {
            '-' if chars.peek().is_some_and(|&(_, n)| n == '>') => {
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

/// `<A as B>::rest` -> `A::rest`, `<A>::rest` -> `A::rest`, recursively.
fn qualified_self(symbol: &str) -> Cow<'_, str> {
    let Some(inner_start) = symbol.strip_prefix('<') else {
        return Cow::Borrowed(symbol);
    };
    let mut depth = 1usize;
    let mut close = None;
    let mut as_at = None;
    let bytes = inner_start.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'-' if bytes.get(i + 1) == Some(&b'>') => i += 1,
            b'<' => depth += 1,
            b'>' => {
                depth -= 1;
                if depth == 0 {
                    close = Some(i);
                    break;
                }
            }
            b' ' if depth == 1 && as_at.is_none() && inner_start.get(i..i + 4) == Some(" as ") => {
                as_at = Some(i);
            }
            _ => {}
        }
        i += 1;
    }
    let Some(close) = close else {
        return Cow::Borrowed(symbol);
    };
    let self_type = inner_start
        .get(..as_at.unwrap_or(close))
        .unwrap_or_default();
    let rest = inner_start.get(close + 1..).unwrap_or_default();
    Cow::Owned(format!("{}{rest}", qualified_self(self_type)))
}

fn is_legacy_hash(segment: &str) -> bool {
    segment.len() == 17
        && segment.starts_with('h')
        && segment.bytes().skip(1).all(|b| b.is_ascii_hexdigit())
}

/// Cuts `text` to at most `max` bytes on a char boundary, saying how long it was.
pub(crate) fn cap(text: &str, max: usize) -> Cow<'_, str> {
    if text.len() <= max {
        return Cow::Borrowed(text);
    }
    let marker = format!("…[{} bytes in all]", text.len());
    let mut end = max.saturating_sub(marker.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!("{}{marker}", text.get(..end).unwrap_or_default()))
}

/// A record's key-values and body, compacted under one byte budget.
pub(crate) struct Record<'a> {
    pairs: Vec<(log::kv::Key<'a>, Slot<'a>)>,
    pub body: Option<String>,
}

enum Slot<'a> {
    Kept(log::kv::Value<'a>),
    Owned(String),
}

impl<'a> Record<'a> {
    pub(crate) fn new(record: &'a log::Record<'a>) -> Self {
        // The origin, found in any value, is written last; everything else shares the
        // budget left after reserving its room.
        let shared = RECORD_MAX - ORIGIN_MAX;
        let mut used = 0;
        let mut origin = None;

        let body = match record.args().as_str() {
            Some(s) if !s.contains(['\n', '\r']) && s.len() <= BODY_MAX => {
                used += s.len();
                None
            }
            Some(s) => Some(own(s, BODY_MAX, shared, &mut used, &mut origin)),
            None => {
                let rendered = record.args().to_string();
                if !rendered.contains(['\n', '\r']) && rendered.len() <= BODY_MAX {
                    used += rendered.len();
                    None
                } else {
                    Some(own(&rendered, BODY_MAX, shared, &mut used, &mut origin))
                }
            }
        };

        let mut collect = Collect { pairs: Vec::new() };
        let _ = record.key_values().visit(&mut collect);

        // Error values are served first, so a long tail of other kvs can't starve
        // them; the output keeps the record's own order.
        let mut slots: Vec<Option<Slot<'a>>> = collect.pairs.iter().map(|_| None).collect();
        let mut dropped = 0usize;
        for errors_pass in [true, false] {
            for (i, (key, value)) in collect.pairs.iter().enumerate() {
                let error = is_error_key(key.as_str());
                if error != errors_pass {
                    continue;
                }
                if used + key.as_str().len() + 1 + MIN_SLICE > shared {
                    dropped += 1;
                    continue;
                }
                used += key.as_str().len() + 1;
                let max = if error { ERROR_VALUE_MAX } else { VALUE_MAX };
                let slot = if let Some(s) = value.to_borrowed_str() {
                    if !s.contains(['\n', '\r']) && s.len() <= max && used + s.len() <= shared {
                        used += s.len();
                        Slot::Kept(value.clone())
                    } else {
                        Slot::Owned(own(s, max, shared, &mut used, &mut origin))
                    }
                } else if value.to_i64().is_some()
                    || value.to_u64().is_some()
                    || value.to_f64().is_some()
                    || value.to_bool().is_some()
                {
                    used += 8;
                    Slot::Kept(value.clone())
                } else {
                    Slot::Owned(own(&value.to_string(), max, shared, &mut used, &mut origin))
                };
                if let Some(entry) = slots.get_mut(i) {
                    *entry = Some(slot);
                }
            }
        }

        let mut pairs: Vec<(log::kv::Key<'a>, Slot<'a>)> = collect
            .pairs
            .into_iter()
            .zip(slots)
            .filter_map(|((key, _), slot)| Some((key, slot?)))
            .collect();
        if dropped > 0 {
            pairs.push((
                log::kv::Key::from_str(DROPPED_KEY),
                Slot::Owned(format!(
                    "{dropped} values over the {RECORD_MAX}-byte record budget"
                )),
            ));
        }
        if let Some(origin) = origin {
            let text = budgeted(&origin, ORIGIN_MAX, RECORD_MAX, &mut used);
            pairs.push((log::kv::Key::from_str(ORIGIN_KEY), Slot::Owned(text)));
        }
        Record { pairs, body }
    }

    /// The compacted pairs, borrowing their owned strings.
    pub(crate) fn pairs(&self) -> Vec<(log::kv::Key<'_>, log::kv::Value<'_>)> {
        self.pairs
            .iter()
            .map(|(k, slot)| {
                let value = match slot {
                    Slot::Kept(v) => v.clone(),
                    Slot::Owned(s) => log::kv::Value::from(s.as_str()),
                };
                (k.clone(), value)
            })
            .collect()
    }
}

fn own(
    text: &str,
    max: usize,
    limit: usize,
    used: &mut usize,
    origin: &mut Option<String>,
) -> String {
    let line = line(text, max);
    if origin.is_none() {
        *origin = line.origin;
    }
    budgeted(&line.text, max, limit, used)
}

fn budgeted(text: &str, max: usize, limit: usize, used: &mut usize) -> String {
    let text = cap(text, limit.saturating_sub(*used).min(max)).into_owned();
    *used += text.len();
    text
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

    #[test]
    fn a_backtrace_nested_inside_a_chain_keeps_the_text_after_it() {
        // `bail!("... {e:?}")` puts a whole Debug rendering, backtrace included, into a message.
        let nested = "outer\n\nCaused by:\n    0: wrapped inner\n\nStack backtrace:\n   0: bipa::a::f\n             at ./src/a.rs:1:1\n\n    1: root cause\n\nStack backtrace:\n   0: bipa::b::g";
        let out = line(nested, ERROR_VALUE_MAX);
        assert_eq!(out.text, "outer: wrapped inner: root cause");
        assert_eq!(out.origin.as_deref(), Some("bipa::a::f"));
    }

    #[test]
    fn multi_line_messages_and_continuations_join_with_spaces() {
        let text = "first line\nsecond line\n\nCaused by:\n    0: cause starts\n       and continues\n    1: root";
        assert_eq!(
            line(text, 1024).text,
            "first line second line: cause starts and continues: root"
        );
    }

    #[test]
    fn plain_text_is_untouched_and_control_characters_become_spaces() {
        assert_eq!(line("all on one line", 64).text, "all on one line");
        assert_eq!(line("tab\there\r\nnext", 64).text, "tab here next");
        assert_eq!(line("ansi \u{1b}[31mred", 64).text, "ansi  [31mred");
    }

    #[test]
    fn a_message_that_merely_mentions_the_header_keeps_its_text() {
        let text = "why\nStack backtrace:\nnot a frame";
        assert_eq!(line(text, 64).text, "why not a frame");
    }

    #[test]
    fn caps_cut_on_char_boundaries_and_say_how_much_was_there() {
        let text = "ação ".repeat(100);
        let out = cap(&text, 64);
        assert!(out.len() <= 64, "{}", out.len());
        assert!(out.ends_with(&format!("…[{} bytes in all]", text.len())));
        assert_eq!(cap("short", 64), "short");
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
    fn origin_keeps_at_most_eight_distinct_frames_within_its_cap() {
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
    }

    #[test]
    fn hostile_input_never_panics() {
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
        ] {
            let _ = line(text, 8);
            let _ = backtrace_origin(text);
            let _ = simplify(text);
            let _ = cap(text, 1);
        }
    }

    fn compacted(
        args: std::fmt::Arguments<'_>,
        kvs: &[(&str, log::kv::Value<'_>)],
        f: impl FnOnce(&Record<'_>, Vec<(String, String)>),
    ) {
        let record = log::Record::builder()
            .level(log::Level::Error)
            .args(args)
            .key_values(&kvs)
            .build();
        let compact = Record::new(&record);
        let pairs = compact
            .pairs()
            .into_iter()
            .map(|(k, v)| (k.as_str().to_owned(), v.to_string()))
            .collect();
        f(&compact, pairs);
    }

    #[test]
    fn a_record_with_a_backtrace_gets_one_line_values_and_an_origin() {
        compacted(
            format_args!("balances failed"),
            &[
                ("user_id", log::kv::Value::from(42)),
                ("e", log::kv::Value::from(OUTAGE)),
            ],
            |record, pairs| {
                assert!(record.body.is_none());
                let e = &pairs.iter().find(|(k, _)| k == "e").unwrap().1;
                assert!(e.starts_with("balances: failed to get prices for BTC"));
                let origin = &pairs.iter().find(|(k, _)| k == ORIGIN_KEY).unwrap().1;
                assert!(
                    origin.starts_with("bipa::features::liquid_quote_parts::assemble_price_bag")
                );
                assert_eq!(pairs.iter().find(|(k, _)| k == "user_id").unwrap().1, "42");
                assert!(pairs.iter().all(|(_, v)| !v.contains('\n')));
            },
        );
    }

    #[test]
    fn an_interpolated_chain_in_the_body_is_compacted_too() {
        compacted(format_args!("failed: {OUTAGE}"), &[], |record, pairs| {
            let body = record.body.as_deref().unwrap();
            assert!(!body.contains('\n'));
            assert!(body.starts_with("failed: balances: failed to get prices for BTC"));
            assert!(pairs.iter().any(|(k, _)| k == ORIGIN_KEY));
        });
    }

    #[test]
    fn many_large_values_stay_within_the_record_budget() {
        let big = "x".repeat(10_000);
        let values: Vec<String> = (0..30).map(|i| format!("k{i}")).collect();
        let kvs: Vec<(&str, log::kv::Value<'_>)> = values
            .iter()
            .map(|k| (k.as_str(), log::kv::Value::from(big.as_str())))
            .chain([("e", log::kv::Value::from(OUTAGE))])
            .collect();
        compacted(format_args!("{big}"), &kvs, |record, pairs| {
            let total: usize = record.body.as_deref().map_or(0, str::len)
                + pairs
                    .iter()
                    .map(|(k, v)| k.len() + 1 + v.len())
                    .sum::<usize>();
            // The budget, plus the dropped-values counter and the keys it doesn't charge.
            assert!(total <= RECORD_MAX + 128, "{total}");
            assert!(pairs
                .iter()
                .any(|(k, v)| k == DROPPED_KEY && v.ends_with("record budget")));
            // The error kept its chain though thirty kvs came before it, and its place.
            let e = pairs.iter().position(|(k, _)| k == "e").unwrap();
            assert!(pairs[e]
                .1
                .starts_with("balances: failed to get prices for BTC"));
            assert!(pairs[..e].iter().all(|(k, _)| k.starts_with('k')));
        });
    }
}
