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
    // The bases collide (both normalize to `mcp__srv__a_b`), so both names
    // carry the collision suffix...
    assert!(first[0].1.ends_with("_6883201b"));
    assert!(first[1].1.ends_with("_68c0481b"));
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
