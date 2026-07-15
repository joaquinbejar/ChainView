//! Property tests for the configuration surface (`docs/TESTING.md` §3).
//!
//! Two invariants:
//!
//! - the provider-id ↔ environment-segment map is **shell-safe for every valid
//!   id** and **round-trips (`decode ∘ encode == id`) over the collision-free id
//!   space** — ids whose separators are not adjacent (`docs/07-configuration.md`
//!   §5.1). The documented per-character scheme is not a total bijection over
//!   the full grammar (adjacent hyphens collide: `encode("a--") ==
//!   encode("a_")`); that defect is owned by issue #4, which owns the
//!   `ProviderId` grammar (see `encode_segment`'s reversibility note). These
//!   tests assert what the scheme actually guarantees, not the impossible total
//!   bijection.
//! - the `humantime` duration parse is gated by the documented range —
//!   `refresh_interval`/`tick_interval` are accepted iff they land within their
//!   ranges.
//!
//! Failing cases are recorded under `proptest-regressions/` — commit that
//! directory as the first line of defence against a regression of the same
//! shape.

use std::time::Duration;

use chainview::ConfigError;
use chainview::config::{CliOverrides, Config, EnvSource, decode_segment, encode_segment};
use proptest::prelude::*;

/// An empty environment — the loader reads nothing from the process env.
struct EmptyEnv;

impl EnvSource for EmptyEnv {
    fn get(&self, _key: &str) -> Option<String> {
        None
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, max_shrink_iters: 50_000, ..ProptestConfig::default() })]

    /// `decode(encode(id)) == id` for underscore/alphanumeric ids (underscores
    /// double consistently, so they always round-trip — including runs).
    #[test]
    fn prop_provider_segment_roundtrips_underscore_ids(id in "[a-z][a-z0-9_]{1,31}") {
        let segment = encode_segment(&id);
        prop_assert_eq!(decode_segment(&segment), id);
    }

    /// `decode(encode(id)) == id` for hyphenated ids where every hyphen sits
    /// between alphanumeric groups (no adjacent separators) — the collision-free
    /// hyphen domain, covering `my-broker`, `td-ameritrade`, etc.
    #[test]
    fn prop_provider_segment_roundtrips_hyphen_ids(
        id in "[a-z][a-z0-9]{0,7}(-[a-z0-9]{1,7}){0,3}"
    ) {
        let segment = encode_segment(&id);
        prop_assert_eq!(decode_segment(&segment), id);
    }

    /// Every encoded segment is a legal POSIX variable segment for ANY valid id
    /// (this holds even for the collision-prone ids): only `[A-Z0-9_]`, starting
    /// with a letter, so `CHAINVIEW_<SEG>_<KEY>` is always assignable in a shell.
    #[test]
    fn prop_provider_segment_is_shell_safe(id in "[a-z][a-z0-9_-]{1,31}") {
        let segment = encode_segment(&id);
        prop_assert!(!segment.is_empty());
        prop_assert!(
            segment
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        );
        prop_assert!(matches!(segment.chars().next(), Some(c) if c.is_ascii_uppercase()));
    }

    /// Distinct underscore/alphanumeric ids never collide on one segment (the
    /// `'_' → '__'` escape keeps this class injective).
    #[test]
    fn prop_provider_segment_no_collision_underscore_ids(
        a in "[a-z][a-z0-9_]{1,31}",
        b in "[a-z][a-z0-9_]{1,31}",
    ) {
        prop_assume!(a != b);
        prop_assert_ne!(encode_segment(&a), encode_segment(&b));
    }

    /// A `refresh_interval` string is accepted iff its parsed duration is within
    /// `[250ms, 300s]`; otherwise it is a typed `ConfigError::InvalidValue`.
    #[test]
    fn prop_refresh_duration_range_gate(ms in 0u64..400_000) {
        let raw = format!("{ms}ms");
        let cli = CliOverrides { refresh_interval: Some(raw), ..CliOverrides::default() };
        let result = Config::assemble(cli, &EmptyEnv, None);
        let in_range = (250..=300_000).contains(&ms);
        match (in_range, result) {
            (true, Ok(config)) => prop_assert_eq!(config.refresh_interval, Duration::from_millis(ms)),
            (false, Err(ConfigError::InvalidValue { .. })) => {}
            (true, Err(e)) => prop_assert!(false, "in-range duration rejected: {}", e),
            (false, Ok(_)) => prop_assert!(false, "out-of-range duration accepted: {}ms", ms),
            (_, Err(other)) => prop_assert!(false, "unexpected error variant: {}", other),
        }
    }

    /// A `tick_interval` string is accepted iff within `[50ms, 5s]`.
    #[test]
    fn prop_tick_duration_range_gate(ms in 0u64..8_000) {
        let raw = format!("{ms}ms");
        let cli = CliOverrides { tick_interval: Some(raw), ..CliOverrides::default() };
        let result = Config::assemble(cli, &EmptyEnv, None);
        let in_range = (50..=5_000).contains(&ms);
        match (in_range, result) {
            (true, Ok(config)) => prop_assert_eq!(config.tick_interval, Duration::from_millis(ms)),
            (false, Err(ConfigError::InvalidValue { .. })) => {}
            (true, Err(e)) => prop_assert!(false, "in-range tick rejected: {}", e),
            (false, Ok(_)) => prop_assert!(false, "out-of-range tick accepted: {}ms", ms),
            (_, Err(other)) => prop_assert!(false, "unexpected error variant: {}", other),
        }
    }
}
