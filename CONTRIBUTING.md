# Contributing

Open an issue before starting a large provider or viewer change. Keep scientific
format behavior in adapters and do not leak provider wire conventions into MCP
tools.

Pull requests must pass:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
npm --prefix viewer/web-bootstrap ci
npm --prefix viewer/web-bootstrap run check
npm --prefix viewer/web-bootstrap run build
```

Never commit real NEON tokens or private ecological datasets. Test fixtures must
be synthetic, redistributable, and small.

