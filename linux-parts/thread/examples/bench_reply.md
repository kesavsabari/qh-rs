## What changed

The relay now keeps **one** connection per peer and retries with *backoff*. The fix is in `src/relay.rs`, with the config read from `qh.toml`.

1. Open the socket once and reuse it.
2. Retry on `ECONNRESET` with jitter.
3. Log the endpoint, see [the docs](https://example.com/qh).

- quiet by default
- loud only on failure
  - nested detail

> A dropped peer is not an error: it reconnects on the next message.

```rust
fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_millis(200);
    base * 2u32.pow(attempt.min(6)) // capped at 12.8 s
}
```

Run `cargo test relay` to check it.
