# ChainView

**ChainView** is a terminal UI for options traders: live option chains, Greeks,
volatility surfaces and payoff diagrams, plus a replay mode for backtest
results — rendered with [ratatui](https://ratatui.rs).

The market-data clients and all the options math live upstream. This crate is
the terminal around them: provider adapters, normalization, and the render loop.

```bash
cargo install chainview
chainview
```

That is the whole setup for the default path. Deribit's public data needs no
credentials, so a stock install opens on a live BTC chain.

## Two modes

**Live** — real-time option chains, Greeks and volatility surfaces streamed
through provider adapters.

```bash
chainview --provider deribit --underlying BTC
```

**Replay** — renders an [IronCondor](https://github.com/joaquinbejar/IronCondor)
backtest result bundle read-only: equity curve, P&L attribution by Greek,
per-trade drill-down and the payoff of the position at the scrub head. No
network.

```bash
chainview replay ./path/to/result-bundle
```

## Screens

| Screen | What it shows |
|--------|---------------|
| Chain | The strike matrix — bid/ask/mark, IV and Greeks per call and put, with the leg drill-down |
| Depth | The order-book ladder, where the provider actually has one |
| Payoff | A multi-leg strategy you build in the terminal, at expiration and t+0, with break-evens |
| Surface | The vol smile, one Greek curve at a time, and the single-expiry surface |
| Replay | Equity curve, Greek attribution, fills and the payoff at the head |

`?` opens the help overlay with every key. `q` quits, and the terminal is
restored on every exit path — a normal quit, an error, or a panic.

## Providers

Not every venue does everything, and ChainView says so rather than filling the
gaps with plausible numbers. Each adapter declares its capabilities, and the UI
gates screens off that declaration; a screen a provider cannot feed renders an
explicit unavailable state.

| Provider | Feature flag | Chain | Depth | Greeks/IV | Status |
|----------|-------------|-------|-------|-----------|--------|
| `deribit` | *(default)* | assembled from instruments | yes | from the venue | ships, zero-config, no credentials |
| `alpaca` | `alpaca` | native | crypto spot only | from the venue | ships |
| `ig` | `ig` | from the navigation tree | unverified | computed locally | ships |
| `ibkr` | `ibkr` | assembled | no | from the venue | ships, needs a running TWS/Gateway |
| `tastytrade` | `tastytrade` | native | no | from the venue | **not shipped** — held on an upstream security gate |
| `dxlink` | `dxlink` | none (overlay only) | no | from the venue | **not shipped** — held on an upstream security gate |

The feature flags are dependency-weight gates: a default build pulls only what
the Deribit path needs. The two gated adapters are compiled but deliberately
never registered while their upstream credential-logging gate holds; enabling
one is a typed startup error, never a silent activation.

Credentials come from the environment only, are never logged, never written to a
config file, and never echoed in an error. See
[`.env.example`](./.env.example) for the key names.

## Extending it with your own venue

`chainview` is a binary **and** a library. The provider port — the `Provider`
trait, the `ProviderCapabilities` self-declaration and the normalized types the
trait emits — is a public, semver-governed surface, so integrating a broker
needs no fork:

```rust,ignore
ChainViewApp::builder()
    .with_builtins()
    .register(MyBroker::new())
    .run()?;
```

The port types, the trait contract and a full worked adapter are in the [crate
documentation](https://docs.rs/chainview). A reserved built-in id or a duplicate
registration is a typed startup error, never a panic.

## Status

`v0.1.0` — the first release with an implementation behind it. Live and replay
modes, five screens and six provider adapters are in the tree, with the two
noted above deliberately withheld. Expect the API to move: the provider port is
versioned from here, but it is a `0.x` line.

## Ecosystem

Part of a family of Rust crates for options trading infrastructure:
[OptionStratLib](https://github.com/joaquinbejar/OptionStratLib) (the chain
model, Greeks, payoff and curve math) ·
[IronCondor](https://github.com/joaquinbejar/IronCondor) (the backtester whose
bundles replay mode reads) ·
[OrderBook-rs](https://github.com/joaquinbejar/OrderBook-rs) ·
[Option-Chain-OrderBook](https://github.com/joaquinbejar/Option-Chain-OrderBook)

## Contribution and Contact

### **Contact Information**
- **Author**: Joaquín Béjar García
- **Email**: jb@taunais.com
- **Telegram**: [@joaquin_bejar](https://t.me/joaquin_bejar)
- **Repository**: <https://github.com/joaquinbejar/ChainView>
- **Documentation**: <https://docs.rs/chainview>

## License

MIT: see [LICENSE](./LICENSE).

<!-- related-projects:start -->
## Related projects

Repositories by the same author that this project depends on, and repositories that depend on it.

### Depends on

| Repository | Description |
|------------|-------------|
| [DXlink](https://github.com/joaquinbejar/DXlink) | DXLink WebSocket client that streams tastytrade real-time quotes and Greeks into the chain view. |
| [OptionStratLib](https://github.com/joaquinbejar/OptionStratLib) | Options pricing, Greeks and volatility models used to build and enrich the option chains shown on screen. |
| [alpaca-rs](https://github.com/joaquinbejar/alpaca-rs) | Alpaca Markets client used by the Alpaca provider for option chain snapshots and streaming market data. |
| [deribit-http](https://github.com/joaquinbejar/deribit-http) | Deribit REST client used by the Deribit provider to load instruments and option chain snapshots. |
| [deribit-websocket](https://github.com/joaquinbejar/deribit-websocket) | Deribit WebSocket client used by the Deribit provider for real-time ticker updates. |
| [ig-client](https://github.com/joaquinbejar/ig-client) | IG broker client used by the IG provider for market data over REST and Lightstreamer streaming. |
| [tastytrade](https://github.com/joaquinbejar/tastytrade) | tastytrade API client used by the tastytrade provider to fetch option chains and authenticate streaming. |

<!-- related-projects:end -->
