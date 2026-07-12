# ChainView

> 🚧 **Early development** — v0.0.1 reserves the crate name. APIs do not exist yet.

**ChainView** is a terminal UI for options traders: live option chains,
Greeks, volatility surfaces and payoff diagrams, rendered with
[ratatui](https://ratatui.rs).

## Planned features

- **Live mode**: real-time option chains and Greeks from Deribit, tastytrade
  (DXLink), IG and Alpaca.
- **Replay mode**: visualize [IronCondor](https://github.com/joaquinbejar/IronCondor)
  backtest results — equity curve, P&L attribution by Greek, per-trade drill-down.
- **Payoff diagrams**: multi-leg strategy payoffs at expiration and t+0,
  straight in the terminal.
- **Zero config**: `cargo install chainview` and go.

## Ecosystem

Part of a family of Rust crates for options trading infrastructure:
[OrderBook-rs](https://github.com/joaquinbejar/OrderBook-rs) ·
[OptionStratLib](https://github.com/joaquinbejar/OptionStratLib) ·
[IronCondor](https://github.com/joaquinbejar/IronCondor) ·
[Option-Chain-OrderBook](https://github.com/joaquinbejar/Option-Chain-OrderBook)

## License

MIT — see [LICENSE](./LICENSE).

## Contact

Joaquin Bejar — jb@taunais.com
