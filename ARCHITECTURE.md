# Architecture

proxy-fire is an OpenAI-compatible proxy that pools the free tiers
of many LLM providers behind a single `localhost:8080/v1` endpoint. This
document describes how the code is organized and why.

## Workspace layout

```
crates/
  core/       Pure domain logic. No HTTP, no I/O except catalog/config files.
              Heavily unit-tested. Everything else depends on this.
  providers/  One module per provider behind a single `Provider` trait.
              The fallback chain talks only to the trait — never to
              provider-specific code.
  proxy/      HTTP layer: axum routes, middleware, fallback chain, SSE.
  scan/       Discovery: scans provider model APIs, writes catalog.json.
  cli/        The one all-in-one binary: `proxy-fire` (serve by default; `scan` and `bench` subcommands).
```

Dependency direction is one-way: `core ← providers ← proxy ← cli` and
`core ← providers ← scan ← cli`. `core` never depends on anything.

## Request flow

```
client ──POST /v1/chat/completions──▶ auth middleware ──▶ routes
                                          │
                                  ┌───────▼────────┐
                                  │  agent profile  │  model: "<profile>" → strategy/pin
                                  └───────┬────────┘
                                  ┌───────▼────────┐
                                  │ response cache  │  non-stream HIT → 200
                                  └───────┬────────┘
                    ┌─────────────────────┴─────────────────────┐
                    │ specific model id                        │
                    ▼                                          ▼
          direct provider lookup              strategy chain ("auto" / strategy / profile)
          (catalog entries, same path)        strategy.rank() → ordered candidates
                    │                                          │
                    └───────────────┬──────────────────────────┘
                                    ▼
                        FallbackChain::execute
        for each candidate (max attempts, per-provider model cap):
          1. skip cooldowns (model / provider / key slot)
          2. pick key slot (round-robin; slot on cooldown skipped)
          3. Gemini: reserve quota slot (4-dim)   Cerebras: pre-emptive spacing
          4. POST {model, ...} (stream stripped — providers always get buffered JSON)
          5. classify: 200+content → Done │ 200 empty → next model
                       429 → cool slot, try next key │ 401/403 → cool slot (key) or provider
                       404 → model cooldown │ 400/413/422 → skip provider
                       else → next model (provider stays eligible)
                                    │
                    all failed ─────▼────────────────────────────
                        openrouter/free ultimate fallback
                                    │
                    exhausted → 503 + Retry-After + X-Proxy-Exhausted
```

Streaming clients get a buffered JSON completion re-emitted as SSE chunks
(`write_completion_as_sse`). The chain always receives buffered JSON so it
can fail over; there is no token-level streaming through fallback.

## Key design decisions

- **One attempt path.** Every upstream chat call goes through
  `FallbackChain::attempt`: admit (cooldowns, quotas, spacing) → reserve →
  call → record. Strategy chains, the direct route and OpenRouter's
  `models[]` call all use it. No duplicate retry logic anywhere.
- **Providers are plugins.** A provider declares its own auth, discovery,
  quota/headers handling and body tweaks. The chain has zero
  `if provider == "gemini"` special cases; e.g. Gemini's `<thought>` stripping
  is a `Provider::post_process_response` hook.
- **State that survives restarts** lives in
  `~/.proxy-fire/{catalog.json, usage.json, reliability.json}`.
  Atomic writes (tmp + rename), save every 5 min and on shutdown.
- **Hot reload** via `notify` watchers on config and catalog files.
- **Empty 200s are failures.** Reasoning models sometimes spend the whole
  token budget on hidden reasoning; the chain tries the next model.
- **Credit tiers rank last** in the `economical` strategy; non-chat models
  (embedders, guards, `nano-banana`…) never route.

## Invariants

- The proxy never forwards `stream: true` to a provider.
- A 429 cools one key slot, never the whole provider, when keys are pooled.
- A 401/403 cools one key slot (10 min); whole provider only with one key.
- A 404 sets a model cooldown (1 h); a 400 a model cooldown (1 h) or
  provider-skip for the request.
- `max_attempts` bounds upstream calls; OpenRouter's ultimate fallback may
  exceed it by one call.
- Catalog entries from failed providers are kept on rescan (`KeepUnscanned`).

## Testing

`cargo test --workspace` runs unit tests per crate plus integration tests
with in-process mock providers (axum on an ephemeral port). No network, no
API keys required.
