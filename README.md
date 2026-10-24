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
