//! Namespaced, deterministic public naming.
//!
//! Every external tool registers as `mcp__<server>__<raw>`, normalized to
//! the provider function-name contract (length and character limits), with
//! a deterministic hash appended when normalization would collide. The
//! public name is a *pure function* of `(server, raw)`: connection order,
//! re-syncs, and other servers never rename anything. The raw name is what
//! goes on the wire; the public name never does.

use std::collections::BTreeSet;

/// Provider function-name character limit.
pub const MAX_NAME_LEN: usize = 64;

/// The separator joining `mcp`, the server, and the raw tool name.
pub const SEPARATOR: &str = "__";

/// Normalize one name segment to the provider character contract:
/// `[A-Za-z0-9_]`, runs of anything else collapsed to a single `_`,
/// leading/trailing separators trimmed.
fn normalize_segment(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    let mut pending = false;
    for ch in segment.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            if pending {
                out.push('_');
                pending = false;
            }
            out.push(ch);
        } else {
            pending = true;
        }
    }
    out
}

/// Deterministic FNV-1a 64-bit digest over the *raw* `(server, raw)` pair,
/// rendered as 8 lowercase hex digits. No external hasher: the collision
/// suffix must be stable across processes and runs.
fn suffix(server: &str, raw: &str) -> String {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x100_0000_01b3;
    let mut hash = OFFSET;
    for byte in server
        .bytes()
        .chain(std::iter::once(b'\x00'))
        .chain(raw.bytes())
    {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")[..8].to_string()
}

/// The base public name for `(server, raw)`: `mcp__<server>__<raw>` with
/// each segment normalized, then the whole name trimmed to the length
/// limit (the prefix `mcp__` is never sacrificed below the separator).
pub fn public_name(server: &str, raw: &str) -> String {
    let joined = format!(
        "mcp{SEPARATOR}{}{SEPARATOR}{}",
        normalize_segment(server),
        normalize_segment(raw)
    );
    if joined.len() <= MAX_NAME_LEN {
        return trim_separators(&joined);
    }
    // Truncate the tail (the raw segment loses length last-to-first is
    // wrong — the *whole* name is capped, so cut from the end) and trim
    // any separator the cut exposed.
    let mut cut = &joined[..MAX_NAME_LEN];
    if let Some(idx) = cut.rfind(SEPARATOR) {
        // Do not end the name with a dangling separator run.
        if idx + SEPARATOR.len() == cut.len() {
            cut = &cut[..idx];
        }
    }
    cut.to_string()
}

fn trim_separators(name: &str) -> String {
    let mut out = name;
    while out.ends_with(SEPARATOR) {
        out = &out[..out.len() - SEPARATOR.len()];
    }
    while out.starts_with(SEPARATOR) {
        out = &out[SEPARATOR.len()..];
    }
    out.to_string()
}

/// Deterministically disambiguate a set of raw names for one server.
///
/// Returns `(raw, public)` pairs in the input order. Suffixing is decided
/// from the `(server, raw)` pair *alone* — never from batch membership:
/// any raw whose base public name is lossy (normalization or truncation
/// changed it, so other raws could share it) carries `_<hash>` (8 hex
/// digits of the FNV-1a digest of the raw pair). A tool's public name is
/// therefore stable across re-syncs even when a colliding sibling
/// disappears. If the suffixed name would breach the length limit, the
/// base is shortened to make room for the suffix.
pub fn public_names(server: &str, raws: &[String]) -> Vec<(String, String)> {
    let mut used: BTreeSet<String> = BTreeSet::new();
    raws.iter()
        .map(|raw| {
            let base = public_name(server, raw);
            let lossy = lossy_pair(server, raw);
            let name = if lossy {
                let tagged = with_suffix(&base, &suffix(server, raw));
                // A deterministic suffix must never be dropped: if two
                // suffixed names still collide (astronomically unlikely
                // and only within one batch), the later one takes the
                // next ordinal so the batch stays injective.
                let mut name = tagged.clone();
                let mut ordinal = 2;
                while used.contains(&name) {
                    name = format!(
                        "{}~{ordinal}",
                        &tagged[..tagged.len().min(MAX_NAME_LEN - 2)]
                    );
                    ordinal += 1;
                }
                name
            } else {
                base
            };
            used.insert(name.clone());
            (raw.clone(), name)
        })
        .collect()
}

/// Whether `public_name(server, raw)` had to alter the pair: a character
/// was rewritten/dropped, or the name was truncated to the length limit.
/// A lossy base is not a faithful rendering of `(server, raw)` — another
/// raw could normalize to the same thing — so it carries the suffix.
/// Decided from the pair alone, never from batch membership.
fn lossy_pair(server: &str, raw: &str) -> bool {
    let joined = format!(
        "mcp{SEPARATOR}{}{SEPARATOR}{}",
        normalize_segment(server),
        normalize_segment(raw)
    );
    let trimmed = trim_separators(&joined);
    // Truncation: the untrimmed join exceeded the cap.
    let truncated = joined.len() > MAX_NAME_LEN;
    // Rewritten: the normalized segments differ from the raw inputs, or
    // trimming removed characters.
    let rewritten =
        normalize_segment(server) != server || normalize_segment(raw) != raw || trimmed != joined;
    truncated || rewritten
}

/// Append `_<hash>`, shortening `base` as needed to stay within the limit.
fn with_suffix(base: &str, hash: &str) -> String {
    let room = MAX_NAME_LEN.saturating_sub(hash.len() + 1);
    let mut head = &base[..base.len().min(room)];
    // Never end on a dangling separator run before appending.
    while head.ends_with(SEPARATOR) {
        head = &head[..head.len() - SEPARATOR.len()];
    }
    format!("{head}_{hash}")
}
