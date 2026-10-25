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
- `POST /api/ingest` takes `{user_id, items:[{source, external_id, author, text, url?, score?}]}` (max 500). **Publishing is opt-in per user**: it returns 404 for an unknown user and 403 unless the user has enabled publishing. It dedupes on (source, external_id), classifies by keyword (`src/classify.rs`) into existing public feeds, and returns `{ingested, duplicates, unclassified, rejected}`. Unclassified posts are not stored.
- `PUT /api/users/{user}/publishing` with `{"enabled": bool}` turns publishing on/off (default off). Turning it off withdraws everything the user contributed (`withdrawn` in the response), except posts another opted-in user also contributed.
- `GET /api/users/{user}/subscriptions` includes `publish_opt_in`.
- `trend_liker/forus.py`: `set_publishing(user, enabled)` and `publish_posts(client, user, posts)` push X posts in; posts from protected accounts are never sent, and the server refuses (403) users who haven't opted in.
- `trend_liker/forus.py`: `ForUsClient` (HTTP) and `ForUsConnector` (plugs into `Registry.query()` alongside the X source).

```
cd universal-feed && cargo run      # serves http://127.0.0.1:3000
```

### Storage

By default the server keeps everything in memory. Set `DATABASE_URL` to use Postgres instead: migrations (`universal-feed/migrations/`) run on startup, and demo data is seeded only into an empty database (`SEED_DEMO=0` disables). `(source, external_id)` has a unique index, so ingest dedupe is enforced by the database even under concurrent writers. Home feeds are capped at 500 items in both backends.

```
DATABASE_URL=postgres://user:pass@localhost:5432/feed cargo run
```

Tests: `pytest` (Python) and `cd universal-feed && cargo test` (Rust). Set `TEST_DATABASE_URL` to also run the Postgres tests (each uses its own throwaway schema and leaves it behind; use a scratch database); without it they are skipped.

Note: republishing X content into shared feeds is subject to X's developer terms on redistribution/display; check them before exposing this beyond local use.
