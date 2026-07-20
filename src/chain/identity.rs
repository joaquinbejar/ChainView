//! Provider-agnostic option identity (`docs/01-domain-model.md` §4).
//!
//! These are the identity types every adapter and store keys on:
//!
//! - [`InstrumentKey`] — the **within-provider storage/merge key** and the
//!   **candidate** cross-feed match, `(underlying, expiry, strike, style)`,
//!   deliberately carrying **no** [`ProviderId`] so a REST snapshot row and a
//!   stream overlay for the same option collapse to one map entry.
//! - [`ContractSpecFingerprint`] — the economic-equivalence spec that gates a
//!   *cross-provider* overlay merge (a 4-tuple match is necessary but not
//!   sufficient; the merge itself lands with the chain store, issue #7).
//! - [`Instrument`] — a key plus the feed that owns this view of it (its
//!   `provider`, native/stream aliases, and `spec`); equality and hashing
//!   delegate to `key` **only**.
//! - [`ProviderId`] — an **open, validated** provider identity newtype; the
//!   six built-in ids in [`RESERVED_PROVIDER_IDS`] are reserved for the
//!   bundled adapters.

use std::hash::{Hash, Hasher};

use chrono::{DateTime, Utc};
use optionstratlib::OptionStyle;
use optionstratlib::prelude::Positive;
use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// ChainView's provider-agnostic option identity: the `(underlying, expiry,
/// strike, style)` tuple every streaming update matches against and every
/// merge / latest-value map keys on.
///
/// **Within** one feed it is the row key the poll→stream merge uses. **Across**
/// feeds it is the *candidate* match for a cross-provider overlay — but a
/// 4-tuple match does **not** prove the two contracts are economically fungible
/// (multiplier, settlement/exercise convention, quote currency and venue
/// product can differ), so a cross-provider overlay is additionally gated by a
/// matching [`ContractSpecFingerprint`] (`docs/01-domain-model.md` §4).
///
/// The key deliberately carries **no** [`ProviderId`]: feed ownership is an
/// attribute of [`Instrument`], never part of the equality/hash identity, so a
/// standalone overlay update and a seeded chain row for the same option compare
/// equal.
///
/// `Eq`/`Hash` are derived over **all four** fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InstrumentKey {
    /// Normalized, upper-case underlying ticker (`"BTC"`, `"SPY"`).
    pub underlying: String,
    /// The option expiry as an **absolute UTC instant** — never a wall-clock-
    /// relative offset. The adapter resolves a relative `ExpirationDate::Days`
    /// to an absolute instant at the seam (issue #15) before it reaches a key.
    pub expiration_utc: DateTime<Utc>,
    /// The strike price, in the contract's quote currency (a non-negative
    /// `optionstratlib` `Positive`).
    pub strike: Positive,
    /// Call or put (`optionstratlib::OptionStyle`).
    pub style: OptionStyle,
}

/// The economic-equivalence fingerprint that gates a **cross-provider** overlay
/// merge (`docs/01-domain-model.md` §4).
///
/// The [`InstrumentKey`] 4-tuple proves two feeds *name* the same option; this
/// fingerprint proves the two contracts are the **same economic instrument**
/// before a live overlay quote is allowed to overwrite a source-chain leg. It
/// is **not** part of the store key (two feeds still share one `InstrumentKey`
/// entry) — it is compared explicitly at overlay-join time (issue #7), where a
/// mismatch is a typed [`crate::OverlayError`] and the leg's overlay is refused,
/// never silently merged.
///
/// `Eq`/`Hash` are derived so the overlay gate can compare it by value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContractSpecFingerprint {
    /// Contracts→underlying units, `> 0` (e.g. `100` for a standard equity
    /// option, `1` for a Deribit inverse contract).
    pub contract_multiplier: u32,
    /// Cash- or physically-settled.
    pub settlement: SettlementStyle,
    /// European- or American-exercise.
    pub exercise: ExerciseStyle,
    /// The currency the premium is quoted in — ISO 4217, e.g. `"USD"`.
    pub quote_currency: String,
    /// The venue's product/root code for this contract.
    pub venue_product_code: String,
}

/// How an option contract settles at expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SettlementStyle {
    /// Cash-settled: the intrinsic value is paid in the quote currency.
    Cash,
    /// Physically-settled: the underlying is delivered.
    Physical,
}

/// When an option contract may be exercised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ExerciseStyle {
    /// European exercise: only at expiry.
    European,
    /// American exercise: any time up to expiry.
    American,
}

/// An [`InstrumentKey`] plus the feed that owns this view of it — its native
/// symbol(s) and contract-spec fingerprint (`docs/01-domain-model.md` §4).
///
/// Equality and hashing delegate to [`key`](Instrument::key) **only**: the
/// `provider`, the native/stream aliases, and the `spec` never participate, so
/// two feeds' views of one contract (a source chain plus a stream overlay)
/// collapse to a single identity. The `spec` is compared **explicitly** at
/// overlay-join time (issue #7), not through `Eq`.
#[derive(Debug, Clone)]
pub struct Instrument {
    /// The provider-agnostic identity — the sole equality/hash anchor.
    pub key: InstrumentKey,
    /// Which feed produced THIS view (an attribute, not part of identity).
    pub provider: ProviderId,
    /// The REST/OCC/`instrument_name` id, for subscription and drill-down.
    pub native_symbol: String,
    /// The stream (dxfeed/…) id when it differs from `native_symbol`.
    pub stream_symbol: Option<String>,
    /// The economic-equivalence fingerprint — gates a cross-provider overlay
    /// merge (issue #7); never part of the store key.
    pub spec: ContractSpecFingerprint,
}

impl PartialEq for Instrument {
    /// Two `Instrument`s are equal iff their [`InstrumentKey`]s are equal — the
    /// `provider`, aliases, and `spec` are deliberately excluded.
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Instrument {}

impl Hash for Instrument {
    /// Hash delegates to [`key`](Instrument::key) alone, consistent with
    /// [`PartialEq`].
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

/// The provider ids ChainView reserves for its bundled adapters. An external
/// registration that reuses one is a typed startup error (the registry lands in
/// issue #12); [`ProviderId::is_reserved`] reports membership. `ibkr` (issue
/// #120) was reserved pre-1.0 so the growth is a minor, not a major (SEMVER.md
/// reserved-id-growth rule).
pub const RESERVED_PROVIDER_IDS: [&str; 6] =
    ["deribit", "tastytrade", "dxlink", "ig", "alpaca", "ibkr"];

/// An **open, validated** market-data provider identity — the registry key, the
/// config namespace segment, and the log label for an adapter
/// (`docs/01-domain-model.md` §4, [ADR-0006](https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0006-open-provider-extension.md)).
///
/// Any developer can mint one for their own adapter; the inner string IS the
/// config/log/wire form, and equality, hashing, and ordering delegate to it.
/// It carries no credential — the id is public, non-secret.
///
/// # Grammar
///
/// The invariant is `^[a-z][a-z0-9]*(?:[_-][a-z0-9]+)*$` with a total length of
/// 2–32 characters: it starts with a lowercase letter and allows `-`/`_` only
/// **isolated between alphanumerics** — no leading, trailing, or adjacent
/// separators. This is a pre-v0.1 tightening (a strict **narrowing** — the
/// accepted set is a proper subset) of the earlier
/// `^[a-z][a-z0-9_-]{1,31}$` grammar, which makes the
/// id ↔ environment-segment transliteration (`docs/07-configuration.md` §5.1) a
/// **total bijection** — see
/// [ADR-0008](https://github.com/joaquinbejar/ChainView/blob/main/docs/adr/0008-provider-id-grammar-and-env-bijection.md).
///
/// # Not matched on
///
/// Because the set is **open**, nothing in the codebase may `match` on a
/// `ProviderId`: the UI gates screens off declared `ProviderCapabilities`, never
/// off the id, so a built-in and an external provider are treated identically
/// (the arch/property test that enforces this lands in issue #22).
///
/// # `serde`
///
/// `serde` round-trips it through its inner string with the same grammar
/// re-validated on the way in (`#[serde(try_from = "String", into = "String")]`),
/// so a malformed id in a persisted/config value is a deserialize error, never a
/// silent bad key.
///
/// # Not `Copy`
///
/// It owns a `String`, so moving it into an `Instrument`/`ChainViewError`/
/// `ConfigError` **clones** it — a deliberate cost of the open model.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ProviderId(String);

impl ProviderId {
    /// Validate and construct a provider id from any string-like value.
    ///
    /// # Errors
    ///
    /// [`ConfigError::InvalidValue`] with `field: "provider id"` when the value
    /// does not match `^[a-z][a-z0-9]*(?:[_-][a-z0-9]+)*$` (2–32 chars, isolated
    /// separators).
    #[must_use = "a validated provider id must be used"]
    pub fn new(id: impl Into<String>) -> Result<Self, ConfigError> {
        let id = id.into();
        if is_valid_provider_id(&id) {
            Ok(Self(id))
        } else {
            Err(ConfigError::InvalidValue {
                field: "provider id".to_owned(),
                reason: format!(
                    "`{id}` must match ^[a-z][a-z0-9]*(?:[_-][a-z0-9]+)*$ \
                     (2-32 chars, no leading/trailing/adjacent `-`/`_`)"
                ),
            })
        }
    }

    /// The id as a string slice — its canonical config/log/wire form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True for exactly the six ids reserved for the built-in adapters
    /// ([`RESERVED_PROVIDER_IDS`]).
    #[must_use]
    pub fn is_reserved(&self) -> bool {
        RESERVED_PROVIDER_IDS.contains(&self.0.as_str())
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for ProviderId {
    type Error = ConfigError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ProviderId> for String {
    fn from(value: ProviderId) -> Self {
        value.0
    }
}

/// The tightened grammar check: 2–32 characters, first a lowercase letter, the
/// rest lowercase letters / digits / isolated `-`/`_` separators (no leading,
/// trailing, or adjacent separator). See [`ProviderId`]'s grammar note and
/// ADR-0008.
fn is_valid_provider_id(id: &str) -> bool {
    let len = id.chars().count();
    if !(2..=32).contains(&len) {
        return false;
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    let mut prev_was_separator = false;
    for c in chars {
        if c == '-' || c == '_' {
            if prev_was_separator {
                return false; // adjacent separators are rejected (ADR-0008)
            }
            prev_was_separator = true;
        } else if c.is_ascii_lowercase() || c.is_ascii_digit() {
            prev_was_separator = false;
        } else {
            return false; // any other character (uppercase, punctuation, …)
        }
    }
    // A trailing separator leaves the flag set — reject it.
    !prev_was_separator
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Test constructors (no unwrap/expect/indexing per the ruleset) -------

    #[track_caller]
    fn pid(id: &str) -> ProviderId {
        match ProviderId::new(id) {
            Ok(p) => p,
            Err(e) => panic!("expected a valid provider id `{id}`, got: {e}"),
        }
    }

    #[track_caller]
    fn utc(secs: i64) -> DateTime<Utc> {
        match DateTime::<Utc>::from_timestamp(secs, 0) {
            Some(t) => t,
            None => panic!("invalid test timestamp: {secs}"),
        }
    }

    #[track_caller]
    fn strike(value: f64) -> Positive {
        match Positive::new(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test strike `{value}`: {e}"),
        }
    }

    fn sample_key() -> InstrumentKey {
        InstrumentKey {
            underlying: "BTC".to_owned(),
            expiration_utc: utc(1_700_000_000),
            strike: strike(60_000.0),
            style: OptionStyle::Call,
        }
    }

    fn sample_spec(multiplier: u32) -> ContractSpecFingerprint {
        ContractSpecFingerprint {
            contract_multiplier: multiplier,
            settlement: SettlementStyle::Cash,
            exercise: ExerciseStyle::European,
            quote_currency: "USD".to_owned(),
            venue_product_code: "BTC".to_owned(),
        }
    }

    fn hash_of<T: Hash>(value: &T) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    // --- Instrument equality delegates to key --------------------------------

    #[test]
    fn test_instrument_eq_collapses_same_key_different_provider_symbol_spec() {
        let key = sample_key();
        let rest = Instrument {
            key: key.clone(),
            provider: pid("deribit"),
            native_symbol: "BTC-27JUN25-60000-C".to_owned(),
            stream_symbol: None,
            spec: sample_spec(1),
        };
        let stream = Instrument {
            key,
            provider: pid("dxlink"),
            native_symbol: ".BTC250627C60000".to_owned(),
            stream_symbol: Some("dxfeed-symbol".to_owned()),
            spec: sample_spec(100),
        };
        // Different provider, aliases, and spec — yet equal and hash-equal,
        // because equality delegates to `key` only.
        assert_eq!(rest, stream);
        assert_eq!(hash_of(&rest), hash_of(&stream));
    }

    #[test]
    fn test_instrument_ne_when_keys_differ() {
        let base = Instrument {
            key: sample_key(),
            provider: pid("deribit"),
            native_symbol: "a".to_owned(),
            stream_symbol: None,
            spec: sample_spec(1),
        };
        let other_key = InstrumentKey {
            strike: strike(61_000.0),
            ..sample_key()
        };
        let other = Instrument {
            key: other_key,
            provider: pid("deribit"),
            native_symbol: "a".to_owned(),
            stream_symbol: None,
            spec: sample_spec(1),
        };
        assert_ne!(base, other);
    }

    // --- InstrumentKey inequality across each of the four fields -------------

    #[test]
    fn test_instrument_key_ne_across_underlying() {
        let a = sample_key();
        let b = InstrumentKey {
            underlying: "ETH".to_owned(),
            ..sample_key()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn test_instrument_key_ne_across_expiration() {
        let a = sample_key();
        let b = InstrumentKey {
            expiration_utc: utc(1_700_086_400),
            ..sample_key()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn test_instrument_key_ne_across_strike() {
        let a = sample_key();
        let b = InstrumentKey {
            strike: strike(60_500.0),
            ..sample_key()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn test_instrument_key_ne_across_style() {
        let a = sample_key();
        let b = InstrumentKey {
            style: OptionStyle::Put,
            ..sample_key()
        };
        assert_ne!(a, b);
    }

    #[test]
    fn test_instrument_key_eq_and_hash_equal_when_all_fields_match() {
        let a = sample_key();
        let b = sample_key();
        assert_eq!(a, b);
        assert_eq!(hash_of(&a), hash_of(&b));
    }

    // --- ContractSpecFingerprint value equality ------------------------------

    #[test]
    fn test_contract_spec_fingerprint_eq_by_value() {
        assert_eq!(sample_spec(100), sample_spec(100));
        assert_ne!(sample_spec(100), sample_spec(1));
    }

    // --- RESERVED_PROVIDER_IDS -----------------------------------------------

    #[test]
    fn test_reserved_provider_ids_membership_is_exactly_six() {
        assert_eq!(RESERVED_PROVIDER_IDS.len(), 6);
        for id in ["deribit", "tastytrade", "dxlink", "ig", "alpaca", "ibkr"] {
            assert!(pid(id).is_reserved(), "`{id}` should be reserved");
        }
    }

    #[test]
    fn test_provider_id_is_reserved_false_for_custom_id() {
        assert!(!pid("my-broker").is_reserved());
    }

    // --- ProviderId grammar: accept ------------------------------------------

    #[test]
    fn test_provider_id_accepts_all_built_in_ids() {
        for id in RESERVED_PROVIDER_IDS {
            assert!(ProviderId::new(id).is_ok(), "`{id}` should be valid");
        }
    }

    #[test]
    fn test_provider_id_accepts_documented_examples() {
        for id in ["my-broker", "my_broker", "td-ameritrade"] {
            assert!(ProviderId::new(id).is_ok(), "`{id}` should be valid");
        }
    }

    // --- ProviderId grammar: reject ------------------------------------------

    #[test]
    fn test_provider_id_rejects_uppercase() {
        assert!(ProviderId::new("Deribit").is_err());
    }

    #[test]
    fn test_provider_id_rejects_leading_digit() {
        assert!(ProviderId::new("1broker").is_err());
    }

    #[test]
    fn test_provider_id_rejects_empty() {
        assert!(ProviderId::new("").is_err());
    }

    #[test]
    fn test_provider_id_rejects_too_short() {
        assert!(ProviderId::new("a").is_err());
    }

    #[test]
    fn test_provider_id_rejects_too_long() {
        // 33 characters — one past the 32-char ceiling.
        assert!(ProviderId::new("a".repeat(33)).is_err());
    }

    #[test]
    fn test_provider_id_accepts_max_length() {
        // Exactly 32 characters.
        assert!(ProviderId::new("a".repeat(32)).is_ok());
    }

    #[test]
    fn test_provider_id_rejects_leading_separator() {
        assert!(ProviderId::new("-broker").is_err());
        assert!(ProviderId::new("_broker").is_err());
    }

    #[test]
    fn test_provider_id_rejects_trailing_separator() {
        assert!(ProviderId::new("broker-").is_err());
        assert!(ProviderId::new("broker_").is_err());
    }

    #[test]
    fn test_provider_id_rejects_adjacent_separators() {
        // The collision defect resolved by ADR-0008: adjacent separators are no
        // longer valid ids, so `encode("a--")` / `encode("a_")` can never arise.
        assert!(ProviderId::new("a--b").is_err());
        assert!(ProviderId::new("a__b").is_err());
        assert!(ProviderId::new("a-_b").is_err());
        assert!(ProviderId::new("a_-b").is_err());
    }

    #[test]
    fn test_provider_id_rejects_non_ascii() {
        // A non-ASCII code point (here U+00E9), written as an escape so the
        // source file stays ASCII, must be rejected by the grammar.
        assert!(ProviderId::new("brok\u{00e9}r").is_err());
    }

    #[test]
    fn test_provider_id_new_error_names_field_provider_id() {
        match ProviderId::new("Bad") {
            Err(ConfigError::InvalidValue { field, .. }) => assert_eq!(field, "provider id"),
            other => panic!("expected InvalidValue on provider id, got {other:?}"),
        }
    }

    // --- ProviderId accessors + ordering -------------------------------------

    #[test]
    fn test_provider_id_as_str_returns_inner() {
        assert_eq!(pid("deribit").as_str(), "deribit");
    }

    #[test]
    fn test_provider_id_ordering_delegates_to_inner_string() {
        assert!(pid("alpaca") < pid("deribit"));
    }

    // --- serde (via the TryFrom/Into the derive delegates to) ----------------

    #[test]
    fn test_provider_id_try_from_string_revalidates() {
        assert!(ProviderId::try_from("deribit".to_owned()).is_ok());
        assert!(ProviderId::try_from("Deribit".to_owned()).is_err());
    }

    #[test]
    fn test_provider_id_into_string_returns_inner() {
        let raw: String = pid("my-broker").into();
        assert_eq!(raw, "my-broker");
    }

    #[test]
    fn test_provider_id_serde_roundtrips_through_string() {
        #[derive(Serialize, Deserialize)]
        struct Wrap {
            id: ProviderId,
        }
        let rendered = match toml::to_string(&Wrap {
            id: pid("my-broker"),
        }) {
            Ok(s) => s,
            Err(e) => panic!("serialize failed: {e}"),
        };
        assert!(rendered.contains("my-broker"));
        match toml::from_str::<Wrap>(&rendered) {
            Ok(w) => assert_eq!(w.id.as_str(), "my-broker"),
            Err(e) => panic!("deserialize failed: {e}"),
        }
    }

    #[test]
    fn test_provider_id_serde_rejects_malformed_on_deserialize() {
        #[derive(Deserialize)]
        struct Wrap {
            #[allow(dead_code)]
            id: ProviderId,
        }
        // Uppercase id in a config value must fail to deserialize, not become a
        // silent bad key.
        assert!(toml::from_str::<Wrap>("id = \"BadId\"\n").is_err());
    }
}
