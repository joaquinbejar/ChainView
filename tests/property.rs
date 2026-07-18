//! Property tests for the provider-id surface and the configuration ranges
//! (`docs/TESTING.md` §3).
//!
//! Invariants:
//!
//! - **`provider_id_grammar`** — [`ProviderId::new`] accepts **exactly** the
//!   tightened grammar `^[a-z][a-z0-9]*(?:[_-][a-z0-9]+)*$` (2–32 chars, isolated
//!   separators; issue #4, `docs/adr/0008-provider-id-grammar-and-env-bijection.md`)
//!   and rejects everything else as a typed `ConfigError`. `serde` round-trips
//!   only valid ids and re-validates on the way in.
//! - **Env-segment bijection** — with the tightened grammar the provider-id ↔
//!   environment-segment map (`docs/07-configuration.md` §5.1) is a **total
//!   bijection over the full valid-id space**: `decode ∘ encode == id` and
//!   distinct ids never collide, for every valid id (the earlier
//!   isolated-separator scoping is gone). Every encoded segment is a legal POSIX
//!   variable segment.
//! - **Range gate** — the `humantime` duration parse is accepted iff within the
//!   documented `refresh_interval` / `tick_interval` ranges.
//!
//! Failing cases are recorded under `proptest-regressions/` — commit that
//! directory as the first line of defence against a regression of the same
//! shape.

use std::collections::BTreeMap;
use std::time::Duration;

use chainview::config::{
    CliOverrides, Config, EnvSource, Secret, decode_segment, encode_segment, provider_env_var,
    require_credentials,
};
use chainview::{
    AuthKind, ChainCapability, ChainPollCapability, ConfigError, GreeksCapability,
    OptionStreamCapability, ProviderCapabilities, ProviderId,
};
use proptest::prelude::*;
use serde::{Deserialize, Serialize};

/// An empty environment — the loader reads nothing from the process env.
struct EmptyEnv;

impl EnvSource for EmptyEnv {
    fn get(&self, _key: &str) -> Option<String> {
        None
    }
}

/// A map-backed environment for deterministic per-provider credential resolution.
struct MapEnv(BTreeMap<String, String>);

impl EnvSource for MapEnv {
    fn get(&self, key: &str) -> Option<String> {
        self.0.get(key).cloned()
    }
}

/// A wrapper so a bare `ProviderId` can be round-tripped through a
/// self-describing serde format (TOML needs a top-level table).
#[derive(Serialize, Deserialize)]
struct Wrap {
    id: ProviderId,
}

/// An independent, split-based oracle for the tightened `ProviderId` grammar —
/// deliberately implemented differently from the production scanner so a
/// divergence in either is caught: a valid id has length 2–32, only
/// `[a-z0-9_-]`, a lowercase-letter first char, and no leading/trailing/adjacent
/// separator (equivalently, every group between separators is non-empty).
fn grammar_oracle(s: &str) -> bool {
    let len = s.chars().count();
    if !(2..=32).contains(&len) {
        return false;
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return false;
    }
    if !matches!(s.chars().next(), Some(c) if c.is_ascii_lowercase()) {
        return false;
    }
    if s.starts_with(['-', '_']) || s.ends_with(['-', '_']) {
        return false;
    }
    !s.split(['-', '_']).any(|group| group.is_empty())
}

/// A proptest strategy over the tightened valid-id grammar, generating both `-`
/// and `_` separators isolated between alphanumeric groups. Lengths land in
/// `[2, 32]`.
const VALID_ID: &str = "[a-z][a-z0-9]{1,7}(?:[_-][a-z0-9]{1,7}){0,3}";

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, max_shrink_iters: 50_000, ..ProptestConfig::default() })]

    /// `ProviderId::new` accepts every id the grammar admits and preserves its
    /// string form.
    #[test]
    fn prop_provider_id_grammar_accepts_valid(id in VALID_ID) {
        match ProviderId::new(&id) {
            Ok(provider) => prop_assert_eq!(provider.as_str(), id.as_str()),
            Err(e) => prop_assert!(false, "valid id `{}` rejected: {}", id, e),
        }
    }

    /// `ProviderId::new` accepts EXACTLY the grammar: over a broad alphabet
    /// (uppercase, digits, separators, any length) it succeeds iff the
    /// independent oracle agrees. A rejection is always a typed
    /// `ConfigError::InvalidValue`.
    #[test]
    fn prop_provider_id_grammar_matches_oracle(s in "[a-zA-Z0-9_-]{0,36}") {
        let expected = grammar_oracle(&s);
        match ProviderId::new(&s) {
            Ok(_) => prop_assert!(expected, "id `{}` accepted but oracle rejects", s),
            Err(ConfigError::InvalidValue { field, .. }) => {
                prop_assert!(!expected, "id `{}` rejected but oracle accepts", s);
                prop_assert_eq!(field, "provider id");
            }
            Err(other) => prop_assert!(false, "unexpected error variant: {}", other),
        }
    }

    /// `decode(encode(id)) == id` for EVERY valid id — the map is a total
    /// bijection over the tightened grammar (no isolated-separator scoping).
    #[test]
    fn prop_provider_segment_roundtrips_all_valid_ids(id in VALID_ID) {
        let segment = encode_segment(&id);
        prop_assert_eq!(decode_segment(&segment), id);
    }

    /// Distinct valid ids never collide on one environment segment.
    #[test]
    fn prop_provider_segment_no_collision_all_valid_ids(a in VALID_ID, b in VALID_ID) {
        prop_assume!(a != b);
        prop_assert_ne!(encode_segment(&a), encode_segment(&b));
    }

    /// Every encoded segment is a legal POSIX variable segment — only
    /// `[A-Z0-9_]`, starting with a letter — so `CHAINVIEW_<SEG>_<KEY>` is always
    /// assignable in a shell.
    #[test]
    fn prop_provider_segment_is_shell_safe(id in VALID_ID) {
        let segment = encode_segment(&id);
        prop_assert!(!segment.is_empty());
        prop_assert!(
            segment
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        );
        prop_assert!(matches!(segment.chars().next(), Some(c) if c.is_ascii_uppercase()));
    }

    /// An external (non-reserved) id resolves its credentials through the SAME
    /// `CHAINVIEW_<SEG>_*` bijection a built-in uses (issue #43,
    /// `docs/03-data-providers.md` §11.3): the loader reads exactly the variable
    /// [`provider_env_var`] builds, for EVERY valid id, using only the public
    /// `chainview::config` surface.
    #[test]
    fn prop_external_id_credentials_resolve_through_bijection(
        id in VALID_ID,
        token in "[A-Za-z0-9._-]{1,24}",
    ) {
        let provider = match ProviderId::new(&id) {
            Ok(p) => p,
            Err(e) => return Err(TestCaseError::fail(format!("valid id rejected: {e}"))),
        };
        let var = provider_env_var(&id, "token");
        let env = MapEnv([(var, token.clone())].into_iter().collect());
        match require_credentials(&env, &provider, &["token"]) {
            Ok(map) => prop_assert_eq!(map.get("TOKEN").map(Secret::expose), Some(token.as_str())),
            Err(e) => prop_assert!(false, "external id `{}` credentials failed to resolve: {}", id, e),
        }
    }

    /// `serde` round-trips a valid `ProviderId` through its string form and
    /// re-validates on the way in.
    #[test]
    fn prop_provider_id_serde_roundtrips_valid(id in VALID_ID) {
        let provider = match ProviderId::new(&id) {
            Ok(p) => p,
            Err(e) => return Err(TestCaseError::fail(format!("valid id rejected: {e}"))),
        };
        let rendered = match toml::to_string(&Wrap { id: provider }) {
            Ok(s) => s,
            Err(e) => return Err(TestCaseError::fail(format!("serialize failed: {e}"))),
        };
        match toml::from_str::<Wrap>(&rendered) {
            Ok(w) => prop_assert_eq!(w.id.as_str(), id.as_str()),
            Err(e) => prop_assert!(false, "deserialize failed: {}", e),
        }
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

// --- Provider-capability builder totality (`capabilities_total`-adjacent) -----
//
// The `capabilities_total` invariant (`docs/TESTING.md` §3) requires every
// registered provider to return a COMPLETE `ProviderCapabilities`. The registry
// lands in issue #12; the adjacent property provable at the port here is that the
// builder ALWAYS yields a complete capability set — for arbitrary values of every
// dimension, `build()` populates all eight fields with exactly the values set, so
// no field can be left indeterminate. This is what makes the builder the safe,
// total, cross-crate construction path.

fn chain_capability_strategy() -> impl Strategy<Value = ChainCapability> {
    prop_oneof![
        Just(ChainCapability::Native),
        Just(ChainCapability::Assemble),
        Just(ChainCapability::Partial),
        Just(ChainCapability::None),
    ]
}

fn greeks_capability_strategy() -> impl Strategy<Value = GreeksCapability> {
    prop_oneof![
        Just(GreeksCapability::Provided),
        Just(GreeksCapability::ComputedLocally),
        Just(GreeksCapability::None),
    ]
}

fn option_stream_strategy() -> impl Strategy<Value = OptionStreamCapability> {
    prop_oneof![
        Just(OptionStreamCapability::None),
        any::<bool>().prop_map(|verified| OptionStreamCapability::SymbolOnly { verified }),
        any::<bool>().prop_map(|verified| OptionStreamCapability::ChainQuotes { verified }),
    ]
}

fn chain_poll_strategy() -> impl Strategy<Value = ChainPollCapability> {
    prop_oneof![
        Just(ChainPollCapability::None),
        any::<u32>()
            .prop_map(|interval_hint_secs| ChainPollCapability::Poll { interval_hint_secs }),
    ]
}

fn auth_kind_strategy() -> impl Strategy<Value = AuthKind> {
    prop_oneof![
        Just(AuthKind::None),
        Just(AuthKind::Token),
        Just(AuthKind::KeySecret),
        Just(AuthKind::UserPass),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, max_shrink_iters: 50_000, ..ProptestConfig::default() })]

    /// For any values of the eight dimensions, `ProviderCapabilities::builder`
    /// yields a complete capability set whose fields equal exactly what was set —
    /// the builder never leaves a dimension indeterminate.
    #[test]
    fn prop_capabilities_builder_yields_complete_set(
        chain in chain_capability_strategy(),
        depth in any::<bool>(),
        greeks in greeks_capability_strategy(),
        option_stream in option_stream_strategy(),
        underlying_stream in any::<bool>(),
        chain_poll in chain_poll_strategy(),
        trades_tape in any::<bool>(),
        auth in auth_kind_strategy(),
    ) {
        let caps = ProviderCapabilities::builder()
            .chain(chain)
            .depth(depth)
            .greeks(greeks)
            .option_stream(option_stream)
            .underlying_stream(underlying_stream)
            .chain_poll(chain_poll)
            .trades_tape(trades_tape)
            .auth(auth)
            .build();
        prop_assert_eq!(caps.chain, chain);
        prop_assert_eq!(caps.depth, depth);
        prop_assert_eq!(caps.greeks, greeks);
        prop_assert_eq!(caps.option_stream, option_stream);
        prop_assert_eq!(caps.underlying_stream, underlying_stream);
        prop_assert_eq!(caps.chain_poll, chain_poll);
        prop_assert_eq!(caps.trades_tape, trades_tape);
        prop_assert_eq!(caps.auth, auth);
    }
}
