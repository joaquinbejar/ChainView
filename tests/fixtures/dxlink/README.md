# DXLink provider fixtures

Recorded-shape payloads that drive the `src/providers/dxlink.rs` normalization
and the cross-provider overlay-pair tests (issue #46, `docs/TESTING.md` §5).

**Constructed-to-wire-shape.** `docs/TESTING.md` §5 asks for fixtures "captured
from the real upstream client and committed verbatim". This environment cannot
open a live DXLink socket, so each event file is **constructed to match the
upstream `dxlink` 0.2.0 wire shape** the adapter parses — the camelCase
`eventType` / `eventSymbol` / `bidPrice` / … field names of
`dxlink::events::QuoteEvent` and `dxlink::events::GreeksEvent`. Each fixture
therefore deserializes into the real upstream event struct and flows through the
adapter's real `map_market_event` → `route_event` decode, so it is a faithful
stand-in for a captured payload. When the upstream client revises a wire shape,
refresh the affected fixture and the pin note in `docs/specs/providers.md` (§0).

The overlay-spec files carry the per-leg `ContractSpecFingerprint` an external
overlay leg declares at composition; they are the matching / mismatched arms of
the cross-provider **overlay pair** (the economic-equivalence gate,
`docs/01-domain-model.md` §4). They are not on-wire DXLink shapes — DXLink events
carry no fingerprint — but the composed overlay leg's fingerprint, committed so
the gate's accept/refuse decision is an auditable contract.

## Files

| Path | Wire shape | Drives |
|------|-----------|--------|
| `quote/quote_symbol.json` | `dxlink::events::QuoteEvent` (`Quote` symbol stream) | `map_market_event` + `route_event` → `QuoteUpdate` (dxlink provider, no venue time) |
| `greeks/greeks_symbol.json` | `dxlink::events::GreeksEvent` (`Greeks` symbol stream) | `map_market_event` + `route_event` → `GreeksRow` (`Provider` origin, IV carried as-is) |
| `overlay/overlay_matching.json` | composed overlay-leg `ContractSpecFingerprint` | overlay pair — fingerprint EQUALS the Deribit source leg → merge, overlay wins |
| `overlay/overlay_mismatched.json` | composed overlay-leg `ContractSpecFingerprint` (degraded) | overlay pair — differing `contract_multiplier` → `OverlayError::SpecMismatch`, refused, source kept, badged |

### The overlay pair

`quote_symbol.json`'s `eventSymbol` (`.BTC250627C60000`) is the dxfeed streamer
symbol of the `BTC-27JUN25-60000-C` option in the committed Deribit source
fixture (`../deribit/instruments/instruments_btc.json`). The overlay-pair test
joins a DXLink overlay leg carrying that stream symbol onto the Deribit source
chain through the domain `AliasCatalog`, then decodes this event and folds it
through the real `ChainStore` gate: under `overlay_matching.json` the leg merges
and the DXLink quote wins; under `overlay_mismatched.json` the leg is refused and
the Deribit source leg is kept.
