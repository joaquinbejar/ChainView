//! The normalized fetch artifact and its per-leg alias catalog
//! (`docs/01-domain-model.md` §6, `docs/03-data-providers.md` §2 and §4).
//!
//! [`ChainFetch`] is the NAMED artifact the provider port's `Provider::fetch_chain`
//! returns — **never** a bare `OptionChain`. A bare chain cannot carry the
//! provider-native and stream symbols the poll→stream merge, subscription,
//! resubscription, and DXLink overlay joins need, so the poll leg returns the
//! chain **plus** its absolute-UTC expiry/source identity ([`ExpirySource`])
//! **plus** the per-leg [`AliasCatalog`].
//!
//! # These are DOMAIN types, re-exported through the port surface
//!
//! `ChainFetch`, `ExpirySource`, and `AliasCatalog` are **domain** types: the
//! `Provider` trait *emits* them and the future `ChainStore` (issue #7)
//! *consumes* them. Because a type used by both the port and the domain must sit
//! in the layer they both depend on, they live under `src/chain/*` (domain) and
//! are re-exported through the provider-port surface
//! (`docs/03-data-providers.md` §11.1). This keeps the compile-time module graph
//! acyclic — port → domain, never domain → port
//! (`docs/03-data-providers.md` §12). They hold no raw upstream DTO.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use optionstratlib::chains::chain::OptionChain;

use super::identity::{
    ContractSpecFingerprint, ExerciseStyle, Instrument, InstrumentKey, ProviderId, SettlementStyle,
};
use crate::error::OverlayError;

/// The NAMED normalized fetch artifact `Provider::fetch_chain` returns
/// (`docs/01-domain-model.md` §6) — **not** a bare `OptionChain`.
///
/// It bundles the normalized `optionstratlib` chain with the two things a bare
/// chain cannot carry: its absolute-UTC expiry/source identity
/// ([`ExpirySource`]) and the per-leg [`AliasCatalog`] that lets the caller
/// (re)subscribe and join later stream updates without re-deriving symbols
/// (`docs/03-data-providers.md` §4). `ChainSnapshot`
/// ([`crate::chain::ChainSnapshot`]) is assembled from a `ChainFetch`, carrying
/// the SAME [`AliasCatalog`] forward with no copy or re-derivation.
#[derive(Debug, Clone)]
pub struct ChainFetch {
    /// The normalized `optionstratlib` chain — the assembled structure, with
    /// (where needed) local Greeks fill-in already applied at the adapter seam.
    pub chain: OptionChain,
    /// The chain's absolute-UTC expiry and the feed that produced this view.
    pub expiry_source: ExpirySource,
    /// The per-leg native+stream symbol index — for subscribe/resubscribe/join
    /// (`docs/03-data-providers.md` §4).
    pub aliases: AliasCatalog,
}

impl ChainFetch {
    /// Construct a fetch artifact from a normalized chain, its expiry/source
    /// identity, and its per-leg alias catalog.
    #[must_use]
    pub fn new(chain: OptionChain, expiry_source: ExpirySource, aliases: AliasCatalog) -> Self {
        Self {
            chain,
            expiry_source,
            aliases,
        }
    }
}

/// The chain's absolute expiry and source identity (`docs/01-domain-model.md`
/// §6).
///
/// The `ChainSnapshot` `chain_key` tuple is built from it. No relative
/// `ExpirationDate::Days` offset ever reaches it — the adapter resolves expiry
/// to an absolute UTC instant at the seam (issue #15,
/// `docs/03-data-providers.md` §3) before it lands here.
#[derive(Debug, Clone)]
pub struct ExpirySource {
    /// The canonical upper-case underlying ticker (`"BTC"`, `"SPY"`).
    pub underlying: String,
    /// The option expiry as an **absolute UTC instant** — never a relative
    /// offset.
    pub expiration_utc: DateTime<Utc>,
    /// The feed that produced THIS chain view (an attribute, not identity).
    pub provider: ProviderId,
}

impl ExpirySource {
    /// Construct a chain expiry/source identity from a canonical underlying, an
    /// absolute-UTC expiry, and the producing provider.
    #[must_use]
    pub fn new(
        underlying: impl Into<String>,
        expiration_utc: DateTime<Utc>,
        provider: ProviderId,
    ) -> Self {
        Self {
            underlying: underlying.into(),
            expiration_utc,
            provider,
        }
    }
}

/// The per-leg alias index the [`ChainFetch`] carries so normalization does not
/// discard the identifiers needed to subscribe, resubscribe, and join later
/// updates (`docs/01-domain-model.md` §6, `docs/03-data-providers.md` §4).
///
/// It maps the provider-agnostic [`InstrumentKey`] to each feed's [`Instrument`]
/// (native + stream symbol **plus its [`ContractSpecFingerprint`]**), holding no
/// raw upstream DTO. One key can map to several `Instrument`s — one per feed
/// that knows this leg (a source chain plus, e.g., a DXLink overlay) — which is
/// how a cross-provider overlay joins a chain seeded by a *different* provider.
///
/// The per-leg fingerprint is what the cross-provider overlay merge compares to
/// refuse an economically non-equivalent leg
/// ([`overlay_compatible`](Self::overlay_compatible)).
#[derive(Debug, Clone, Default)]
pub struct AliasCatalog {
    /// One entry per feed that knows this leg — each carries its own native and
    /// stream symbols and its spec fingerprint. Private so the presence/uniqueness
    /// invariants stay inside this type.
    by_key: HashMap<InstrumentKey, Vec<Instrument>>,
}

impl AliasCatalog {
    /// An empty catalog. Adapters populate it with [`insert`](Self::insert)
    /// during chain normalization.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a feed's view of a leg. Multiple feeds can register the same
    /// [`InstrumentKey`] — each becomes a distinct alias entry under that key.
    ///
    /// Re-inserting the same `(key, provider)` **replaces** the prior entry in
    /// place rather than appending a duplicate, so the catalog stays a function
    /// of `(key, provider)` and [`instrument`](Self::instrument) returns the
    /// current view, never a stale first match.
    pub fn insert(&mut self, instrument: Instrument) {
        let entries = self.by_key.entry(instrument.key.clone()).or_default();
        match entries
            .iter_mut()
            .find(|existing| existing.provider == instrument.provider)
        {
            Some(existing) => *existing = instrument,
            None => entries.push(instrument),
        }
    }

    /// True when no leg has been registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    /// The number of distinct [`InstrumentKey`]s the catalog knows (not the
    /// number of feed aliases).
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    /// The native/stream symbols (and spec fingerprint) to (re)subscribe for one
    /// leg on a given feed, or `None` when that feed does not know the leg.
    #[must_use]
    pub fn instrument(&self, key: &InstrumentKey, provider: &ProviderId) -> Option<&Instrument> {
        self.by_key
            .get(key)?
            .iter()
            .find(|instrument| &instrument.provider == provider)
    }

    /// Resolve an incoming stream/native symbol back to the shared
    /// [`InstrumentKey`] — the reverse join the DXLink overlay and the
    /// poll→stream merge both use (`docs/03-data-providers.md` §4). Matches a
    /// leg's `native_symbol` or its `stream_symbol`.
    #[must_use]
    pub fn resolve_symbol(&self, symbol: &str) -> Option<&InstrumentKey> {
        self.by_key.iter().find_map(|(key, instruments)| {
            let matches = instruments.iter().any(|instrument| {
                instrument.native_symbol == symbol
                    || instrument.stream_symbol.as_deref() == Some(symbol)
            });
            matches.then_some(key)
        })
    }

    /// The cross-provider overlay gate (`docs/01-domain-model.md` §4): the two
    /// feeds' [`Instrument`]s for one key may merge only when their
    /// [`ContractSpecFingerprint`]s are equal.
    ///
    /// The `InstrumentKey` 4-tuple proves the two feeds *name* the same option;
    /// this compares the economic-equivalence fingerprint before a live overlay
    /// quote is allowed to overwrite a source-chain leg. The within-provider
    /// poll→stream merge passes the same provider for `source` and `overlay`, so
    /// it compares one fingerprint against itself — a no-op that always succeeds.
    ///
    /// The store-level *invocation* of this gate during the merge lands with the
    /// `ChainStore` in issue #7; this is the comparison primitive it calls, with
    /// the full fingerprint comparison already wired here.
    ///
    /// # Errors
    ///
    /// [`OverlayError::SpecMismatch`] naming the first fingerprint dimension that
    /// disagrees (multiplier / settlement / exercise / quote currency / venue
    /// product code) — never a raw payload or credential.
    ///
    /// [`OverlayError::MissingAlias`] when either feed does not know the leg:
    /// there is no fingerprint pair to compare, so the gate fails **CLOSED**
    /// (naming the absent feed) rather than admitting an unverified overlay. The
    /// caller (`gate_overlay`) treats only `Ok(())` as permission to merge, so a
    /// missing alias refuses the merge instead of resurrecting an unchecked leg.
    pub fn overlay_compatible(
        &self,
        key: &InstrumentKey,
        source: &ProviderId,
        overlay: &ProviderId,
    ) -> Result<(), OverlayError> {
        let Some(source_leg) = self.instrument(key, source) else {
            return Err(OverlayError::MissingAlias {
                contract: contract_label(key),
                provider: source.clone(),
            });
        };
        let Some(overlay_leg) = self.instrument(key, overlay) else {
            return Err(OverlayError::MissingAlias {
                contract: contract_label(key),
                provider: overlay.clone(),
            });
        };
        compare_fingerprints(&contract_label(key), &source_leg.spec, &overlay_leg.spec)
    }
}

/// A non-secret, human-readable label for a leg — used only in an
/// [`OverlayError`] message, never for identity. All four components are already
/// normalized, non-secret values.
#[must_use]
fn contract_label(key: &InstrumentKey) -> String {
    format!(
        "{} {} {} {}",
        key.underlying,
        key.expiration_utc.to_rfc3339(),
        key.strike,
        key.style.as_str()
    )
}

/// A stable, non-secret label for a settlement style.
#[must_use]
const fn settlement_label(settlement: SettlementStyle) -> &'static str {
    match settlement {
        SettlementStyle::Cash => "cash",
        SettlementStyle::Physical => "physical",
    }
}

/// A stable, non-secret label for an exercise style.
#[must_use]
const fn exercise_label(exercise: ExerciseStyle) -> &'static str {
    match exercise {
        ExerciseStyle::European => "european",
        ExerciseStyle::American => "american",
    }
}

/// Compare two per-leg fingerprints dimension by dimension, returning the first
/// disagreement as a typed [`OverlayError::SpecMismatch`]. The dimension order
/// matches the field order of [`ContractSpecFingerprint`].
fn compare_fingerprints(
    contract: &str,
    source: &ContractSpecFingerprint,
    overlay: &ContractSpecFingerprint,
) -> Result<(), OverlayError> {
    if source.contract_multiplier != overlay.contract_multiplier {
        return Err(OverlayError::SpecMismatch {
            contract: contract.to_owned(),
            field: "contract_multiplier",
            source: source.contract_multiplier.to_string(),
            overlay: overlay.contract_multiplier.to_string(),
        });
    }
    if source.settlement != overlay.settlement {
        return Err(OverlayError::SpecMismatch {
            contract: contract.to_owned(),
            field: "settlement",
            source: settlement_label(source.settlement).to_owned(),
            overlay: settlement_label(overlay.settlement).to_owned(),
        });
    }
    if source.exercise != overlay.exercise {
        return Err(OverlayError::SpecMismatch {
            contract: contract.to_owned(),
            field: "exercise",
            source: exercise_label(source.exercise).to_owned(),
            overlay: exercise_label(overlay.exercise).to_owned(),
        });
    }
    if source.quote_currency != overlay.quote_currency {
        return Err(OverlayError::SpecMismatch {
            contract: contract.to_owned(),
            field: "quote_currency",
            source: source.quote_currency.clone(),
            overlay: overlay.quote_currency.clone(),
        });
    }
    if source.venue_product_code != overlay.venue_product_code {
        return Err(OverlayError::SpecMismatch {
            contract: contract.to_owned(),
            field: "venue_product_code",
            source: source.venue_product_code.clone(),
            overlay: overlay.venue_product_code.clone(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use optionstratlib::OptionStyle;
    use optionstratlib::prelude::Positive;

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
    fn pos(value: f64) -> Positive {
        match Positive::new(value) {
            Ok(p) => p,
            Err(e) => panic!("invalid test positive `{value}`: {e}"),
        }
    }

    fn sample_key() -> InstrumentKey {
        InstrumentKey {
            underlying: "BTC".to_owned(),
            expiration_utc: utc(1_700_000_000),
            strike: pos(60_000.0),
            style: OptionStyle::Call,
        }
    }

    fn spec(multiplier: u32) -> ContractSpecFingerprint {
        ContractSpecFingerprint {
            contract_multiplier: multiplier,
            settlement: SettlementStyle::Cash,
            exercise: ExerciseStyle::European,
            quote_currency: "USD".to_owned(),
            venue_product_code: "BTC".to_owned(),
        }
    }

    fn instrument(provider: &str, native: &str, stream: Option<&str>) -> Instrument {
        Instrument {
            key: sample_key(),
            provider: pid(provider),
            native_symbol: native.to_owned(),
            stream_symbol: stream.map(str::to_owned),
            spec: spec(1),
        }
    }

    // --- AliasCatalog round-trips --------------------------------------------

    #[test]
    fn test_alias_catalog_new_is_empty() {
        let catalog = AliasCatalog::new();
        assert!(catalog.is_empty());
        assert_eq!(catalog.len(), 0);
        assert!(catalog.resolve_symbol("anything").is_none());
    }

    #[test]
    fn test_alias_catalog_resolves_native_symbol_to_shared_key() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", "BTC-27JUN25-60000-C", None));
        assert_eq!(
            catalog.resolve_symbol("BTC-27JUN25-60000-C"),
            Some(&sample_key())
        );
    }

    #[test]
    fn test_alias_catalog_resolves_stream_symbol_to_shared_key() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument(
            "tastytrade",
            ".SPY250627C600",
            Some(".SPY250627C600:dxfeed"),
        ));
        // The stream symbol resolves to the SAME shared key as the native one.
        assert_eq!(
            catalog.resolve_symbol(".SPY250627C600:dxfeed"),
            Some(&sample_key())
        );
        assert_eq!(
            catalog.resolve_symbol(".SPY250627C600"),
            Some(&sample_key())
        );
    }

    #[test]
    fn test_alias_catalog_resolve_unknown_symbol_is_none() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", "BTC-27JUN25-60000-C", None));
        assert!(catalog.resolve_symbol("UNKNOWN").is_none());
    }

    #[test]
    fn test_alias_catalog_instrument_lookup_by_feed() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", "BTC-27JUN25-60000-C", None));
        catalog.insert(instrument("dxlink", ".BTC250627C60000", Some("dxfeed-sym")));
        let key = sample_key();
        match catalog.instrument(&key, &pid("deribit")) {
            Some(found) => assert_eq!(found.native_symbol, "BTC-27JUN25-60000-C"),
            None => panic!("expected the deribit alias for the leg"),
        }
        match catalog.instrument(&key, &pid("dxlink")) {
            Some(found) => assert_eq!(found.stream_symbol.as_deref(), Some("dxfeed-sym")),
            None => panic!("expected the dxlink alias for the leg"),
        }
        // A feed that never registered the leg has no alias.
        assert!(catalog.instrument(&key, &pid("alpaca")).is_none());
    }

    #[test]
    fn test_alias_catalog_reinsert_same_key_provider_replaces_not_duplicates() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", "OLD-SYMBOL", None));
        catalog.insert(instrument("deribit", "NEW-SYMBOL", Some("new-stream")));
        // The re-insert of the same (key, provider) updated in place: still one
        // distinct key, and `instrument()` returns the CURRENT view, not the
        // stale first match.
        assert_eq!(catalog.len(), 1);
        match catalog.instrument(&sample_key(), &pid("deribit")) {
            Some(found) => {
                assert_eq!(found.native_symbol, "NEW-SYMBOL");
                assert_eq!(found.stream_symbol.as_deref(), Some("new-stream"));
            }
            None => panic!("expected the deribit alias after re-insert"),
        }
        // The stale symbol no longer resolves; the current one does — proving no
        // duplicate lingered under the key.
        assert!(catalog.resolve_symbol("OLD-SYMBOL").is_none());
        assert_eq!(catalog.resolve_symbol("NEW-SYMBOL"), Some(&sample_key()));
        // A different feed under the same key stays a distinct alias.
        catalog.insert(instrument("dxlink", "DX-SYMBOL", Some("dx-stream")));
        assert_eq!(catalog.len(), 1);
        assert!(catalog.instrument(&sample_key(), &pid("dxlink")).is_some());
        assert!(catalog.instrument(&sample_key(), &pid("deribit")).is_some());
    }

    // --- ChainFetch carries the catalog forward unchanged --------------------

    #[test]
    fn test_chain_fetch_carries_alias_catalog_forward_unchanged() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", "BTC-27JUN25-60000-C", None));
        catalog.insert(instrument("dxlink", ".BTC250627C60000", Some("dxfeed-sym")));
        let fetch = ChainFetch::new(
            OptionChain::new("BTC", pos(60_000.0), "2025-06-27".to_owned(), None, None),
            ExpirySource::new("BTC", utc(1_700_000_000), pid("deribit")),
            catalog,
        );
        // The catalog rides forward on the artifact with every lookup intact.
        assert_eq!(fetch.aliases.len(), 1); // one distinct key, two feed aliases
        assert_eq!(
            fetch.aliases.resolve_symbol("dxfeed-sym"),
            Some(&sample_key())
        );
        assert!(
            fetch
                .aliases
                .instrument(&sample_key(), &pid("deribit"))
                .is_some()
        );
        assert_eq!(fetch.expiry_source.underlying, "BTC");
        assert_eq!(fetch.chain.symbol, "BTC");
    }

    // --- overlay_compatible: the fingerprint gate ----------------------------

    #[test]
    fn test_overlay_compatible_same_feed_is_ok() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", "BTC-27JUN25-60000-C", None));
        // Within-provider merge: source == overlay, one fingerprint, always Ok.
        assert!(
            catalog
                .overlay_compatible(&sample_key(), &pid("deribit"), &pid("deribit"))
                .is_ok()
        );
    }

    #[test]
    fn test_overlay_compatible_matching_specs_is_ok() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", "native", None));
        catalog.insert(instrument("dxlink", "native2", Some("stream")));
        // Both were built with spec(1) — equal fingerprints merge.
        assert!(
            catalog
                .overlay_compatible(&sample_key(), &pid("deribit"), &pid("dxlink"))
                .is_ok()
        );
    }

    #[test]
    fn test_overlay_compatible_multiplier_mismatch_is_spec_mismatch() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", "native", None));
        let mut overlay = instrument("dxlink", "native2", Some("stream"));
        overlay.spec = spec(100); // a different contract multiplier
        catalog.insert(overlay);
        match catalog.overlay_compatible(&sample_key(), &pid("deribit"), &pid("dxlink")) {
            Err(OverlayError::SpecMismatch {
                field,
                source,
                overlay,
                ..
            }) => {
                assert_eq!(field, "contract_multiplier");
                assert_eq!(source, "1");
                assert_eq!(overlay, "100");
            }
            other => panic!("expected a multiplier SpecMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_overlay_compatible_settlement_mismatch_is_spec_mismatch() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", "native", None));
        let mut overlay = instrument("dxlink", "native2", Some("stream"));
        overlay.spec = ContractSpecFingerprint {
            settlement: SettlementStyle::Physical,
            ..spec(1)
        };
        catalog.insert(overlay);
        match catalog.overlay_compatible(&sample_key(), &pid("deribit"), &pid("dxlink")) {
            Err(OverlayError::SpecMismatch {
                field,
                source,
                overlay,
                ..
            }) => {
                assert_eq!(field, "settlement");
                assert_eq!(source, "cash");
                assert_eq!(overlay, "physical");
            }
            other => panic!("expected a settlement SpecMismatch, got {other:?}"),
        }
    }

    #[test]
    fn test_overlay_compatible_missing_overlay_leg_is_refused() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("deribit", "native", None));
        // The overlay feed never registered the leg — there is no fingerprint
        // pair to compare, so the gate must fail CLOSED (name the absent feed)
        // rather than admit an unverified overlay.
        match catalog.overlay_compatible(&sample_key(), &pid("deribit"), &pid("dxlink")) {
            Err(OverlayError::MissingAlias { provider, .. }) => {
                assert_eq!(provider.as_str(), "dxlink");
            }
            other => panic!("expected a MissingAlias refusal, got {other:?}"),
        }
    }

    #[test]
    fn test_overlay_compatible_missing_source_leg_is_refused() {
        let mut catalog = AliasCatalog::new();
        catalog.insert(instrument("dxlink", "native", Some("stream")));
        // The source feed never registered the leg — same fail-CLOSED outcome,
        // naming the source feed this time.
        match catalog.overlay_compatible(&sample_key(), &pid("deribit"), &pid("dxlink")) {
            Err(OverlayError::MissingAlias { provider, .. }) => {
                assert_eq!(provider.as_str(), "deribit");
            }
            other => panic!("expected a MissingAlias refusal, got {other:?}"),
        }
    }

    // --- ExpirySource --------------------------------------------------------

    #[test]
    fn test_expiry_source_new_sets_fields() {
        let source = ExpirySource::new("BTC", utc(1_700_000_000), pid("deribit"));
        assert_eq!(source.underlying, "BTC");
        assert_eq!(source.expiration_utc, utc(1_700_000_000));
        assert_eq!(source.provider.as_str(), "deribit");
    }
}
