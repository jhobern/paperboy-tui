# paperboy-tui

Reusable [ratatui](https://docs.rs/ratatui) TUI building blocks, extracted from
[PaperBoy](https://github.com/jhobern/paperboy). Each crate is independent (they
don't depend on one another) and is published separately to crates.io — pick
just the ones you need.

| Crate | What it does |
| --- | --- |
| [`tui-panel-select`](crates/tui-panel-select) | Panel-scoped mouse text selection + cross-platform clipboard copy, with a resize-stable wrap cache. Optional batteries-included mouse handler and a panic-safe terminal guard. |
| [`tui-rgb-picker`](crates/tui-rgb-picker) | A small, configurable RGB colour picker widget (R/G/B channel sliders) — bring your own keys, styling and labels. |
| [`tui-line-editor`](crates/tui-line-editor) | A single- or multi-line text editor primitive: cursor, selection, masking, and scrolling/field renderers. |

Each crate has its own README with examples.

## Repository layout

This is a Cargo workspace; the crates live under `crates/`. Shared metadata
(edition, license, repository, authors) and dependency versions are declared
once in the root `Cargo.toml` via `[workspace.package]` and
`[workspace.dependencies]`, and inherited by each crate.

```sh
cargo build --workspace
cargo test  --workspace
```

## Publishing

Each crate publishes independently (any order — there are no inter-crate
dependencies):

```sh
cargo publish -p tui-panel-select
cargo publish -p tui-rgb-picker
cargo publish -p tui-line-editor
```

## License

MIT — see [LICENSE](LICENSE).
