# trend-liker

Feed -> per-user agent -> federated connector layer, developed against a mock X API.

- `x_client.py` – X API v2 client (home timeline, likes). X has no "For You" endpoint; we use reverse-chronological.
- `mock_x.py` – in-process mock of those endpoints (httpx transport) incl. pagination, 401, 429.
- `agent.py` – `FeedAgent`: scores posts against a user's opted-in interests. **Dry-run by default**; capped likes.
- `connector.py` – `Registry.query()` fans one query across connectors (timeouts, partial failures); `shared_topics()` for cross-user generalization.

```
pip install -e '.[dev]' && pytest
```

Note: X's automation rules restrict bulk/automated liking. Keep it per-user, opt-in, rate-capped, with the user's own OAuth token before going live.

## For Us feeds (`universal-feed/`)

A Rust/Axum server (imported from the supplied zip) where **FOR US** = shared public feeds and **FOR ME** = a user's subscriptions to them. See `universal-feed/README.md`.

- Changed from the original: handlers return 404/400 for unknown users/feeds instead of panicking.
- Its X/TikTok adapters are still placeholders; wiring `x_client.py` output into the server as candidates is the next step.
- `trend_liker/forus.py`: `ForUsClient` (HTTP) and `ForUsConnector` (plugs into `Registry.query()` alongside the X source).

```
cd universal-feed && cargo run      # serves http://127.0.0.1:3000
```
