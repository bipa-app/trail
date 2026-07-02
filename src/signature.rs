//! Stable error fingerprints for novelty alerting: every ERROR record gets an
//! `error.fingerprint` attribute so Loki can alert on signatures never seen
//! before, without Sentry.
//!
//! Identity = fnv1a(module ⊕ message template ⊕ scrubbed error chain). The
//! chain (an anyhow `{:?}` rendering carried in an error-ish kv pair) is
//! truncated before "Stack backtrace:" so line drift across deploys never
//! re-mints a fingerprint; the scrub replaces dynamic tokens (ids, uuids,
//! hashes) so interpolated values don't either. Panic records key on the
//! panic site's file instead of the module, which is constant for every
//! panic (the hook's own log site).

#[derive(Default)]
pub(crate) struct Signature {
    pub fingerprint: String,
    pub scope: String,
    pub root: String,
}

pub(crate) fn compute(record: &log::Record<'_>) -> Signature {
    let mut kv = Collected::default();
    let _ = record.key_values().visit(&mut kv);

    // Native panic records carry the hook's panic_location kv; a user record
    // that merely borrows the "panic" target falls through to the error path.
    if record.target() == crate::init::PANIC_TARGET {
        if let Some(location) = kv.panic_location.as_deref() {
            let scope = strip_line_col(location).to_owned();
            let msg = message_component(record);
            return Signature {
                fingerprint: fnv1a(&["panic", &scope, &msg]),
                root: truncate_chars(&msg, 80).to_owned(),
                scope,
            };
        }
    }

    let scope = record.module_path().unwrap_or_default().to_owned();
    let msg = message_component(record);
    let chain = kv.chain.or(kv.chain_fallback);
    let (chain_component, root) = match chain.as_deref().map(strip_backtrace) {
        Some(chain) => {
            // root must derive from the same capped slice the hash sees, or two
            // chains equal up to the cap could share a fingerprint yet split on root.
            let capped = truncate_chars(chain, 4096);
            (
                scrub(capped),
                truncate_chars(&root_line(capped), 80).to_owned(),
            )
        }
        None => (String::new(), truncate_chars(&msg, 80).to_owned()),
    };
    Signature {
        fingerprint: fnv1a(&[&scope, &msg, &chain_component]),
        scope,
        root,
    }
}

// The static format string when the record had no interpolation (the common
// case); otherwise the rendered first line, scrubbed and stripped of any
// inlined `{e:?}` chain/backtrace text.
fn message_component(record: &log::Record<'_>) -> String {
    if let Some(template) = record.args().as_str() {
        return template.to_owned();
    }
    let rendered = record.args().to_string();
    let first_line = strip_backtrace(&rendered)
        .split("\nCaused by:")
        .next()
        .unwrap_or_default()
        .lines()
        .next()
        .unwrap_or_default();
    scrub(first_line)
}

#[derive(Default)]
struct Collected {
    chain: Option<String>,
    chain_fallback: Option<String>,
    panic_location: Option<String>,
}

impl<'k> log::kv::VisitSource<'k> for Collected {
    fn visit_pair(
        &mut self,
        key: log::kv::Key<'k>,
        value: log::kv::Value<'k>,
    ) -> Result<(), log::kv::Error> {
        match key.as_str() {
            "e" | "err" | "error" | "reason" | "cause" | "why" => {
                if self.chain.is_none() {
                    self.chain = Some(value.to_string());
                }
            }
            "panic_location" => self.panic_location = Some(value.to_string()),
            _ => {
                if self.chain_fallback.is_none() {
                    let rendered = value.to_string();
                    if rendered.contains("\nCaused by:") {
                        self.chain_fallback = Some(rendered);
                    }
                }
            }
        }
        Ok(())
    }
}

// Appends the computed signature to a record's existing key-values.
pub(crate) struct WithSignature<'a> {
    inner: &'a dyn log::kv::Source,
    sig: &'a Signature,
}

impl<'a> WithSignature<'a> {
    pub(crate) fn new(inner: &'a dyn log::kv::Source, sig: &'a Signature) -> Self {
        Self { inner, sig }
    }
}

impl log::kv::Source for WithSignature<'_> {
    fn visit<'k>(
        &'k self,
        visitor: &mut dyn log::kv::VisitSource<'k>,
    ) -> Result<(), log::kv::Error> {
        self.inner.visit(visitor)?;
        if !self.sig.fingerprint.is_empty() {
            visitor.visit_pair(
                log::kv::Key::from_str("error.fingerprint"),
                log::kv::Value::from(self.sig.fingerprint.as_str()),
            )?;
            visitor.visit_pair(
                log::kv::Key::from_str("error.scope"),
                log::kv::Value::from(self.sig.scope.as_str()),
            )?;
            visitor.visit_pair(
                log::kv::Key::from_str("error.root"),
                log::kv::Value::from(self.sig.root.as_str()),
            )?;
        }
        Ok(())
    }
}

fn strip_backtrace(chain: &str) -> &str {
    match chain.find("\nStack backtrace:") {
        Some(i) => chain[..i].trim_end(),
        None => chain.trim_end(),
    }
}

// Last non-empty line of the chain = the root cause; anyhow numbers multi-cause
// chains ("1: text"), so strip that ordinal.
fn root_line(chain: &str) -> String {
    let last = chain
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .trim();
    let stripped = match last.split_once(": ") {
        Some((n, rest)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => rest,
        _ => last,
    };
    scrub(stripped)
}

fn strip_line_col(location: &str) -> &str {
    let mut s = location;
    for _ in 0..2 {
        match s.rfind(':') {
            Some(i) if i + 1 < s.len() && s[i + 1..].bytes().all(|b| b.is_ascii_digit()) => {
                s = &s[..i];
            }
            _ => break,
        }
    }
    s
}

// Replaces dynamic tokens (ids, uuids, hashes, emails, invoices) with "#",
// keeping word-shaped tokens — cron keys and enum variants are the
// discriminators. Whitespace is collapsed so indentation never matters.
fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(4096));
    for token in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        if is_dynamic(token) {
            out.push('#');
        } else {
            out.push_str(token);
        }
    }
    out
}

fn is_dynamic(token: &str) -> bool {
    if token.contains('@') {
        return true;
    }
    if token.len() >= 12
        && ["lnbc", "lnurl", "bc1", "tb1"]
            .iter()
            .any(|prefix| token.starts_with(prefix))
    {
        return true;
    }
    if is_uuid(token.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '-')) {
        return true;
    }
    let core = token.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if core.is_empty() {
        return false;
    }
    let digits = core.chars().filter(|c| c.is_ascii_digit()).count();
    digits == core.chars().count()
        || digits >= 6
        || (core.len() >= 8 && core.chars().all(|c| c.is_ascii_hexdigit()))
        // long mixed alphanumerics are opaque blobs (base64, jwt segments, tokens)
        || (core.len() >= 20 && digits >= 2)
}

fn is_uuid(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(i, &b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

fn truncate_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn fnv1a(parts: &[&str]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        // separator so ("ab", "c") and ("a", "bc") hash differently
        hash ^= 0x1f;
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const TURBO_CHAIN: &str = "Failed renewing turbo plan, user_id: 311396\n\nCaused by:\n    should have configs\n\nStack backtrace:\n   0: <opentelemetry::context::future_ext::WithContext<T> as core::future::future::Future>::poll\n   1: bipa::crons::turbo_plan::renew_and_cancel::{{closure}}";

    fn with_record(
        args: std::fmt::Arguments<'_>,
        target: &str,
        module: &str,
        kvs: &[(&str, log::kv::Value<'_>)],
        f: impl FnOnce(&log::Record<'_>),
    ) {
        f(&log::Record::builder()
            .level(log::Level::Error)
            .target(target)
            .module_path(Some(module))
            .args(args)
            .key_values(&kvs)
            .build());
    }

    fn turbo_signature(chain: &str) -> Signature {
        let mut out = Signature::default();
        with_record(
            format_args!("Renew or cancel failed"),
            "bipa::crons::turbo_plan",
            "bipa::crons::turbo_plan",
            &[("error", log::kv::Value::from(chain))],
            |record| out = compute(record),
        );
        out
    }

    #[test]
    fn real_prod_record_pins_scope_root_and_fingerprint() {
        let sig = turbo_signature(TURBO_CHAIN);
        assert_eq!(sig.scope, "bipa::crons::turbo_plan");
        assert_eq!(sig.root, "should have configs");
        assert_eq!(sig.fingerprint.len(), 16);
        assert!(sig.fingerprint.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[test]
    fn interpolated_ids_do_not_change_the_fingerprint() {
        let a = turbo_signature(TURBO_CHAIN);
        let b = turbo_signature(&TURBO_CHAIN.replace("311396", "999001"));
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn backtrace_drift_does_not_change_the_fingerprint() {
        let a = turbo_signature(TURBO_CHAIN);
        let b = turbo_signature(&TURBO_CHAIN.replace("::poll", "::other_frame"));
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn two_bails_at_one_site_get_two_fingerprints() {
        let a = turbo_signature(TURBO_CHAIN);
        let b = turbo_signature(&TURBO_CHAIN.replace("should have configs", "stale exchange rate"));
        assert_ne!(a.fingerprint, b.fingerprint);
        assert_eq!(b.root, "stale exchange rate");
    }

    #[test]
    fn a_new_middle_context_mints_a_new_fingerprint() {
        let a =
            turbo_signature("outer\n\nCaused by:\n    0: loading sender\n    1: record not found");
        let b =
            turbo_signature("outer\n\nCaused by:\n    0: loading sendee\n    1: record not found");
        assert_ne!(a.fingerprint, b.fingerprint);
        assert_eq!(a.root, "record not found");
    }

    #[test]
    fn identical_chains_collapse_to_one_fingerprint() {
        // sender vs sendee via bare `?`: byte-identical chains are one class.
        let a = turbo_signature("outer\n\nCaused by:\n    record not found");
        let b = turbo_signature("outer\n\nCaused by:\n    record not found");
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn chain_renumbering_alone_does_not_re_mint() {
        let a = turbo_signature("outer\n\nCaused by:\n    0: mid\n    1: root cause");
        let b = turbo_signature("outer\n\nCaused by:\n    1: mid\n    2: root cause");
        assert_eq!(a.fingerprint, b.fingerprint);
    }

    #[test]
    fn chainless_error_degrades_to_module_and_message() {
        let mut sig = Signature::default();
        with_record(
            format_args!("connection pool exhausted"),
            "app",
            "bipa::db",
            &[],
            |record| sig = compute(record),
        );
        assert!(!sig.fingerprint.is_empty());
        assert_eq!(sig.scope, "bipa::db");
        assert_eq!(sig.root, "connection pool exhausted");
    }

    #[test]
    fn interpolated_message_keeps_word_tokens_and_scrubs_ids() {
        let mut sig = Signature::default();
        let key = "bipa_cashback_expense";
        with_record(
            format_args!("Cron {key} failed after 311396 ms"),
            "app",
            "crons",
            &[],
            |record| sig = compute(record),
        );
        assert_eq!(sig.root, "Cron bipa_cashback_expense failed after # ms");
    }

    #[test]
    fn chain_inlined_in_message_is_cut_at_the_chain() {
        let mut sig = Signature::default();
        with_record(
            format_args!("failed to sync: {TURBO_CHAIN}"),
            "app",
            "bipa::ceps",
            &[],
            |record| sig = compute(record),
        );
        assert_eq!(
            sig.root,
            "failed to sync: Failed renewing turbo plan, user_id: #"
        );
    }

    #[test]
    fn panic_records_key_on_the_panic_site_file() {
        let mut a = Signature::default();
        let mut b = Signature::default();
        let mut c = Signature::default();
        let payload = "index out of bounds: the len is 3 but the index is 7";
        for (out, location) in [
            (&mut a, "src/crons/turbo_plan.rs:10:5"),
            (&mut b, "src/crons/turbo_plan.rs:99:1"),
            (&mut c, "src/features/pix/outflows.rs:1529:20"),
        ] {
            with_record(
                format_args!("panic: {payload}"),
                crate::init::PANIC_TARGET,
                "trail::init",
                &[
                    ("kind", log::kv::Value::from("panic")),
                    ("panic_location", log::kv::Value::from(location)),
                    ("backtrace", log::kv::Value::from("   0: frame")),
                ],
                |record| *out = compute(record),
            );
        }
        assert_eq!(a.scope, "src/crons/turbo_plan.rs");
        assert_eq!(
            a.root,
            "panic: index out of bounds: the len is # but the index is #"
        );
        assert_eq!(a.fingerprint, b.fingerprint); // line drift within a file
        assert_ne!(a.fingerprint, c.fingerprint); // different file
    }

    #[test]
    fn user_record_borrowing_the_panic_target_takes_the_error_path() {
        let mut sig = Signature::default();
        with_record(
            format_args!("manual panic-like error"),
            crate::init::PANIC_TARGET,
            "bipa::features::foo",
            &[],
            |record| sig = compute(record),
        );
        assert_eq!(sig.scope, "bipa::features::foo");
        assert_eq!(sig.root, "manual panic-like error");
    }

    #[test]
    fn root_and_fingerprint_agree_beyond_the_chain_cap() {
        // chains identical up to the 4096 cap must share BOTH fingerprint and root
        let long = format!("outer\n\nCaused by:\n    {}", "y".repeat(5000));
        let a = turbo_signature(&format!("{long}\n    tail root one"));
        let b = turbo_signature(&format!("{long}\n    tail root two"));
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.root, b.root);
    }

    #[test]
    fn scrub_pins() {
        assert_eq!(scrub("user_id: 311396"), "user_id: #");
        assert_eq!(scrub("should have configs"), "should have configs");
        assert_eq!(
            scrub("No such file (os error 2)"),
            "No such file (os error #"
        );
        assert_eq!(
            scrub("id 550e8400-e29b-41d4-a716-446655440000 gone"),
            "id # gone"
        );
        assert_eq!(scrub("mail to nick@example.com failed"), "mail to # failed");
        assert_eq!(scrub("at 2026-07-01T19:04:18.260"), "at #");
        assert_eq!(scrub("txid deadbeefcafe1234"), "txid #");
        assert_eq!(scrub("invoice lnbc2500u1pvjluezhash"), "invoice #");
        assert_eq!(scrub("jwt eyJhbGciOiJIUzI1NiJ9 rejected"), "jwt # rejected");
        assert_eq!(
            scrub("word_shaped_cron_key_stays kept"),
            "word_shaped_cron_key_stays kept"
        );
        assert_eq!(scrub("  spaced\n    out  "), "spaced out");
    }

    #[test]
    fn pathological_records_do_not_panic() {
        let mut sig = Signature::default();
        let huge = "x".repeat(200_000);
        with_record(
            format_args!("boom"),
            "app",
            "m",
            &[("error", log::kv::Value::from(huge.as_str()))],
            |record| sig = compute(record),
        );
        assert!(!sig.fingerprint.is_empty());
    }

    #[test]
    fn with_signature_appends_three_pairs() {
        struct Count(usize);
        impl<'k> log::kv::VisitSource<'k> for Count {
            fn visit_pair(
                &mut self,
                _: log::kv::Key<'k>,
                _: log::kv::Value<'k>,
            ) -> Result<(), log::kv::Error> {
                self.0 += 1;
                Ok(())
            }
        }
        let sig = Signature {
            fingerprint: "abc".into(),
            scope: "m".into(),
            root: "r".into(),
        };
        let inner: &[(&str, log::kv::Value<'_>)] = &[("k", log::kv::Value::from("v"))];
        let mut count = Count(0);
        log::kv::Source::visit(&WithSignature::new(&inner, &sig), &mut count).unwrap();
        assert_eq!(count.0, 4);

        let empty = Signature::default();
        let mut count = Count(0);
        log::kv::Source::visit(&WithSignature::new(&inner, &empty), &mut count).unwrap();
        assert_eq!(count.0, 1);
    }

    #[test]
    fn rebuilt_record_carries_original_and_appended_kvs() {
        struct Keys(Vec<String>);
        impl<'k> log::kv::VisitSource<'k> for Keys {
            fn visit_pair(
                &mut self,
                key: log::kv::Key<'k>,
                _: log::kv::Value<'k>,
            ) -> Result<(), log::kv::Error> {
                self.0.push(key.as_str().to_owned());
                Ok(())
            }
        }
        let sig = Signature {
            fingerprint: "f".into(),
            scope: "s".into(),
            root: "r".into(),
        };
        let inner: &[(&str, log::kv::Value<'_>)] = &[("error", log::kv::Value::from("boom"))];
        let record = log::Record::builder()
            .args(format_args!("m"))
            .key_values(&inner)
            .build();
        let kvs = WithSignature::new(record.key_values(), &sig);
        let rebuilt = record.to_builder().key_values(&kvs).build();
        let mut keys = Keys(Vec::new());
        rebuilt.key_values().visit(&mut keys).unwrap();
        assert_eq!(
            keys.0,
            ["error", "error.fingerprint", "error.scope", "error.root"]
        );
    }
}
