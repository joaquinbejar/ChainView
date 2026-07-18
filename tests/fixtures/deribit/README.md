# Deribit provider fixtures

Recorded-shape payloads that drive the `src/providers/deribit.rs` normalization
and transport-lifecycle tests (issue #17, `docs/TESTING.md` §5).

**Constructed-to-wire-shape.** `docs/TESTING.md` §5 asks for fixtures "captured
from the real upstream client and committed verbatim". This environment cannot
open a live Deribit socket, so each file is **constructed to match the upstream
DTO wire shape** at the pinned crate versions the adapter parses:

- `deribit-http` **0.7.1** — `get_instruments` (`Vec<Instrument>`) and the ticker
  object (`TickerData`).
- `deribit-websocket` **0.3.1** — the `ticker.{instrument}` and grouped
  `book.{instrument}.{group}.{depth}.{interval}` subscription-notification `data`
  objects (`TickerPayload` / `BookPayload`). The book leg the adapter subscribes is
  the **grouped** full-snapshot channel (#48); the raw `book.{instrument}.raw` delta
  fixtures below are retained only to prove the defensive decoder still parses that
  legacy shape.

Each field name/type mirrors those upstream structs exactly, so the fixture is a
faithful stand-in for a captured payload. When the upstream client revises a wire
shape, refresh the affected fixture and the pin note in
`docs/specs/providers.md` (§0).

## Files

| Path | Wire shape | Drives |
|------|-----------|--------|
| `instruments/instruments_btc.json` | `deribit-http` `get_instruments` → `Vec<Instrument>` | option filter + `InstrumentKey` mapping + chain assembly |
| `instruments/instruments_missing_strike.json` | `Vec<Instrument>` (degraded) | row-fatal reject: missing strike / unknown style |
| `ticker/ticker_normal.json` | Deribit ticker object (`TickerData` **and** `ticker.` `TickerPayload`) | `normalize_leg` chain row + `normalize_ticker` quote/greeks (IV / 100) |
| `ticker/ticker_zero_bid.json` | `ticker.` `data` (degraded) | zero bid is valid, kept |
| `ticker/ticker_crossed.json` | `ticker.` `data` (degraded) | crossed quote → bid/ask dropped, prior kept |
| `ticker/ticker_negative.json` | `ticker.` `data` (degraded) | negative bid dropped, ask kept |
| `ticker/ticker_non_finite.json` | `ticker.` `data` (degraded) | JSON has no `NaN`/`Inf` literal — a non-numeric string field refuses the whole frame |
| `book/book_grouped_snapshot.json` | grouped `book.{inst}.{group}.{depth}.{interval}` snapshot `data` (`[price, amount]`) → `BookPayload` | **the subscribed shape**: `normalize_book` full ladder + `change_id` |
| `book/book_snapshot.json` | legacy `book.{inst}.raw` snapshot `data` → `BookPayload` | defensive decode of the raw `[action, price, amount]` shape + `change_id` |
| `book/book_delta.json` | legacy `book.{inst}.raw` change `data` → `BookPayload` | defensive decode of a raw delta (`change`/`delete` actions), `change_id` |

### The non-finite decision

JSON cannot represent `NaN`/`Inf` as a numeric literal, so a real degraded
Deribit frame delivers a non-finite price as a non-numeric field (or omits it).
`ticker_non_finite.json` uses a non-numeric string for `best_bid_price`, which
makes `TickerPayload` deserialization fail — the adapter drops the whole frame
(`route_message` returns without publishing), so no fabricated value reaches the
chain. The `f64` `NaN`/`Inf` guards themselves (`positive_or_drop`,
`normalize_iv`) are proven by the in-module property tests, which inject
`f64::NAN`/`f64::INFINITY` directly.
