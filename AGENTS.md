# AGENTS.md — guide for AI agents working on this repo

## What this is

proxy-fire: one Rust binary that pools free LLM provider tiers
behind a single OpenAI-compatible endpoint with automatic fallback.
See `ARCHITECTURE.md` for the design. If code and this file disagree,
code wins — update this file.

## Golden rules

1. **Never** special-case a provider inside the fallback chain. Add it to
   `crates/providers/` instead.
2. **Never** duplicate retry/cooldown/quota logic — it lives in
   `FallbackChain::attempt` (`crates/proxy/src/fallback.rs`).
3. **Never** forward `stream: true` upstream; the chain is buffered.
4. **Never** log API keys, Authorization headers, or request bodies.
5. **Never** silently wipe the catalog — `KeepUnscanned` preserves entries
   from providers whose scan fails.
6. Keep `configs/config.example.yaml` **byte-compatible** — existing
   deployments depend on it.
7. Failure classification is deliberate (see ARCHITECTURE.md flow). Don't
   "simplify" it.

## Build / test / lint

```bash
cargo build --workspace          # fast dev build
cargo test --workspace           # unit + integration (no network needed)
cargo clippy --workspace -- -D warnings
cargo fmt --all --check
cargo build --release            # thin LTO, stripped, opt-level 3
```

## Adding a provider

1. New module `crates/providers/src/<provider>.rs` implementing `Provider`.
2. Register it in `providers::registry()`.
3. Add an entry to `configs/config.example.yaml` (disabled if it lists paid models).
4. Add scan coverage + one integration test in `crates/providers`.

## Adding a strategy

1. Implement `Strategy` in `crates/core/src/strategy/`.
2. Register in `strategy::registry()`.
3. Strategy is config-facing and user-visible — pick a clear name.

## Code style

- Idiomatic Rust: no `unwrap()` outside tests; `thiserror` in libraries,
  `anyhow` in binaries; `tracing` for logs (never `println!`).
- Document *why*, not *what*. Document the non-obvious; match the surrounding style.
- Keep modules small and cohesive; one concept per file.

## Release

- `cargo release` is not required; tags + GitHub Actions build the one
  all-in-one `proxy-fire` binary and the distroless Docker image.
  Keep it to ONE binary — do not add more binaries.
