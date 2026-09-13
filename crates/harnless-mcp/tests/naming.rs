//! Naming contract: public names are pure functions of `(server, raw)`,
//! normalized to the provider function-name contract, deterministic under
//! collision, and never renamed by connection order or re-syncs.

use harnless_mcp::naming::{public_name, public_names, MAX_NAME_LEN};

#[test]
fn public_name_is_prefixed_and_normalized() {
    assert_eq!(public_name("docs", "search"), "mcp__docs__search");
    // Non-contract characters collapse to single underscores.
    assert_eq!(
        public_name("my-server", "get.file"),
        "mcp__my_server__get_file"
    );
}

#[test]
fn public_name_is_a_pure_function_of_server_and_raw() {
    // Property: repeated evaluation, in any order, for any pair, yields the
    // same name. (Pure by construction: no environment, no state.)
    let long_raw = "x".repeat(200);
    let pairs = [
        ("a", "one"),
        ("a", "two"),
        ("b", "one"),
        ("weird server!", "tool/name?x"),
        ("s", long_raw.as_str()),
    ];
    for (server, raw) in pairs {
        let first = public_name(server, raw);
        for _ in 0..8 {
            assert_eq!(
                public_name(server, raw),
                first,
                "purity violated for {server}/{raw}"
            );
        }
    }
    // Order independence: naming one pair never depends on what else was
    // named before.
    let _ = public_name("other", "thing");
    assert_eq!(public_name("a", "one"), "mcp__a__one");
}

#[test]
fn public_name_respects_the_length_limit() {
    let long = "x".repeat(200);
    let name = public_name(&long, &long);
    assert!(
        name.len() <= MAX_NAME_LEN,
        "name exceeded limit: {}",
        name.len()
    );
    assert!(name.starts_with("mcp__"));
    // Never ends on a dangling separator.
    assert!(!name.ends_with("__"));
}

#[test]
fn colliding_bases_get_deterministic_hash_suffixes() {
    // `a-b` and `a_b` normalize identically → collision within one server.
    let raws = vec!["a-b".to_string(), "a_b".to_string()];
    let first = public_names("srv", &raws);
    let second = public_names("srv", &raws);
    // Deterministic: identical assignment on every run (re-syncs never rename).
    assert_eq!(first, second);
    // The lossy raw (`a-b` normalizes away from itself) carries the
    // collision suffix; the faithful raw keeps its clean base. Suffixing
    // is decided from the pair alone, so this assignment cannot change
    // when a sibling disappears from a later batch.
    // Suffix is the FNV digest of the *lossy* pair ("a-b"); the faithful
    // raw ("a_b") keeps its clean base.
    let suffix_dash = {
        const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
        const PRIME: u64 = 0x100_0000_01b3;
        let mut hash = OFFSET;
        for byte in "srv"
            .bytes()
            .chain(std::iter::once(b'\x00'))
            .chain("a-b".bytes())
        {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(PRIME);
        }
        format!("{hash:016x}")[..8].to_string()
    };
    let by_raw: std::collections::HashMap<&str, &str> = first
        .iter()
        .map(|(r, p)| (r.as_str(), p.as_str()))
        .collect();
    // The lossy raw ("a-b": normalization rewrote it) carries the suffix;
    // the faithful raw ("a_b") keeps the clean base.
    assert_eq!(
        by_raw["a-b"],
        format!("mcp__srv__a_b_{suffix_dash}").as_str()
    );
    assert_eq!(by_raw["a_b"], "mcp__srv__a_b");
    // ...and the assignment is injective.
    let distinct: std::collections::HashSet<&str> = first.iter().map(|(_, p)| p.as_str()).collect();
    assert_eq!(distinct.len(), 2, "colliding raws must get distinct names");
    // Distinct raws with distinct bases never collide.
    let raws = vec!["alpha".to_string(), "beta".to_string()];
    let names = public_names("srv", &raws);
    assert_eq!(names[0].1, "mcp__srv__alpha");
    assert_eq!(names[1].1, "mcp__srv__beta");
}

#[test]
fn collision_suffix_stays_within_the_length_limit() {
    // Force collision by making both normalize the same: embed separators.
    let raws = vec!["a-b".repeat(40), "a_b".repeat(40)];
    let names = public_names("srv", &raws);
    for (_, public) in &names {
        assert!(
            public.len() <= MAX_NAME_LEN,
            "suffixed name too long: {}",
            public
        );
    }
    let distinct: std::collections::HashSet<&str> = names.iter().map(|(_, p)| p.as_str()).collect();
    assert_eq!(distinct.len(), 2, "suffixed names must be injective");
}

#[test]
fn other_servers_never_rename_anything() {
    let a = public_name("alpha", "tool");
    let _b = public_name("beta", "tool");
    assert_eq!(public_name("alpha", "tool"), a);
}
