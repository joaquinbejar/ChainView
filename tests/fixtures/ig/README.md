# IG provider fixtures

`docs/TESTING.md` §5, `docs/03-data-providers.md` §7.4 / §8.

## `depth/option_epic_price_snapshot.json` — the option-epic depth fixture (#50)

This is the **option-epic depth fixture** issue #50 requires: the market-details /
ladder-subscription payload for a **dated-option epic**, recorded to answer one
question the capability matrix left open — *does IG populate its five-level depth
ladder for a dated option?* (`docs/03-data-providers.md` §8, the `unverified`
depth cell).

### What it records

The fixture mirrors the `ig-client` 0.12.1 wire shape for the two surfaces the IG
depth path would read:

- `market_details_snapshot` — the `MarketService::get_market_details(epic)` snapshot
  (the poll leg). The option **is** quoted: a top-of-book `bid` / `offer` is present.
- `price_subscription.fields` — a Lightstreamer `PRICE:{epic}` subscription update,
  whose fields are exactly the `ig_client::model::streaming::StreamingPriceField`
  five-level DOM set (`BIDPRICE1..5`, `ASKPRICE1..5`, `BIDSIZE1..5`, `ASKSIZE1..5`,
  `BIDQUOTEID`, `ASKQUOTEID`, `TIMESTAMP`) — the ONLY source of a multi-level ladder.

Every five-level DOM field arrives **`null`**: IG exposes no order-book depth for
OTC dated options (the depth-of-market ladder is an L2 feature of exchange-traded
instruments, e.g. indices / shares, not the dealer-quoted options book). The option
carries a single top-of-book quote (from the market-details snapshot), **not** a
five-level ladder.

### The disposition: SHAPE-ONLY - the cell stays `unverified` pending a recorded payload

The DOM ladder is **unpopulated** for a dated-option epic, so IG has no option
order book to render. But this fixture is HAND-AUTHORED to the documented wire shape,
not a recorded live payload - a hand-authored artifact cannot establish what a live
venue does or does not populate. The honest disposition is **shape-only**: the depth
screen
stays unavailable for IG (it would show its capability-unavailable state, never a
fabricated ladder). This is the valid, honest outcome the matrix footnote
anticipated (`docs/03-data-providers.md` §8: the cell "flips to `yes`/`no` in the
same PR that lands the fixture").

### Why this is evidence-on-file, not a live adapter capture

The IG **built-in adapter now ships** behind the `ig` Cargo feature (issue #39;
`docs/03-data-providers.md` §7.4 banner, §8 note 3) and declares `depth: false` on
this evidence. But this fixture is HAND-AUTHORED to the documented `ig-client` wire
shape, not a RECORDED live payload, so it cannot on its own establish what a live
venue populates. It is therefore committed as a **DATA artifact** — the documented
wire shape plus the observed reality that a dated-option epic does not populate the
DOM ladder — that BACKS the adapter's `depth: false` and keeps the matrix cell
`unverified` (shape-only) until a recorded payload lands:

- Now: the fixture parses as the documented IG DOM wire shape and confirms the
  five-level fields are unpopulated (see `src/tests_capability_matrix.rs`), backing
  the shipped adapter's `depth: false` while the cell stays `unverified` (shape-only)
  pending a RECORDED live payload.
- Issue #50 (v0.5) lands the recorded option-epic payload that flips the cell to a
  verified `no`, driven through the shipped IG adapter's depth path.

The fixture is **not** a live capture from a production IG account (that needs
credentials the zero-config test suite does not carry);
it is faithful to `ig-client`'s models and to IG's documented depth-of-market
availability, and it exists precisely to end the `unverified` ambiguity with a
committed artifact.
