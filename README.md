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

## Lobbies: shared, live re-ranked feeds (`src/lobby.rs`)

A lobby is a "game server" for a feed. Members pool what their **own** platform feeds show them (`share_feed()` pulls each member's X timeline in), and every interaction is a vote that immediately re-ranks the pool for every other member. The server owns the shared ranking, not the content: posts are references back to X.

- **Ranking** for viewer *u*: `(1-a)·consensus + a·personal + overlap + 0.1·platform_score − seen`, where consensus is the decayed sum of all members' votes (one vote per kind per member), personal weights each voter by the cosine similarity of their votes to *u*'s, overlap rewards posts that several members' own feeds surfaced, and seen demotes what *u* already saw. Then authors are diversified and a fraction of slots go to below-the-fold posts (exploration, deterministic per viewer and version).
- **Policy** is the lobby's portable algorithmic identity (JSON, owner-editable): vote weights, half-life, affinity, overlap prior, seen penalty, author cap, exploration, topic filter, capacity.
- **Live**: `GET /api/lobbies/{id}/events?user=` is a Server-Sent Events stream (`hello`, then one event per change, with a version). Clients refetch `GET /api/lobbies/{id}/feed?user=` when the version moves. Events don't say who voted.
- **Consent**: contributing requires the publishing opt-in; opting out or leaving withdraws posts only you contributed (leaving also removes your votes). Only members can read a lobby's feed or events.
- **Finance**: stock posts (keywords or cashtags like `$AAPL`) get no vote, overlap or affinity amplification unless the policy sets `amplify_finance`, and the Python `FeedAgent` never auto-likes them unless `Policy(like_finance=True)`. Cross-user amplification of stock tips is how pump-and-dumps work.

Routes: `POST/GET /api/lobbies`, `GET /api/lobbies/{id}`, `PUT .../policy` (owner), `POST .../members`, `DELETE .../members/{user}`, `POST .../contribute`, `POST .../interactions` (`impression|like|bookmark|reply|skip`, `active: false` retracts), `GET .../feed`, `GET .../events`.

Lobby state is in memory (like a game server's live state), so it resets on restart. Users and consent live in the Repo.
