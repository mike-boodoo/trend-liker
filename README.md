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

- **Access rules** (`policy.access`): posts under a gated label are hidden from a member until they meet a rule for it, based on what they **produced** (their posts under a label that *another* member liked, bookmarked or replied to) or **consumed** (other members' posts under a label they liked, bookmarked or replied to; impressions don't count). `of` counts a different label, e.g. unlock `search` after engaging with 2 `rust` posts:

  ```json
  "access": [
    { "label": "search", "when": { "produced": 1 } },
    { "label": "search", "when": { "consumed": 2, "of": "rust" } }
  ]
  ```

  Rules for one label are alternatives; both counts in one rule must hold. You always see your own posts, and the owner sees everything (moderation, and someone has to vote first so producers can earn access). Access is derived from the current pool, so it lapses if the votes or posts behind it go away. Rules that can't be met (consuming a label to see that same label), unknown labels, and rules that earn access by posting finance content are rejected with 422 and a reason. Hidden posts are absent from feeds and exploration, voting on one returns 404 like a missing post, the feed's `locked` field shows what's still locked and your progress, the public lobby profile omits the pool size, and live events carry no post IDs.

Routes: `POST/GET /api/lobbies`, `GET /api/lobbies/{id}`, `PUT .../policy` (owner), `POST .../members`, `DELETE .../members/{user}`, `POST .../contribute`, `POST .../interactions` (`impression|like|bookmark|reply|skip`, `active: false` retracts), `GET .../feed`, `GET .../events`.

Lobby state is in memory (like a game server's live state), so it resets on restart. Users and consent live in the Repo.

## Authentication: Sign in with X (`src/auth.rs`)

Every API that acts for a user takes the user from the session; a request that names a different user (in a path, `user_id` field or `?user=`) gets 403, and one with no valid session gets 401. Public: `GET /api/feeds`, `GET /api/lobbies`, `GET /api/lobbies/{id}`.

- **Sign-in**: `GET /auth/x/login` starts OAuth 2.0 with PKCE (S256) against X; `GET /auth/x/callback` checks the one-time `state` (10-minute expiry, and bound to the browser that started sign-in by a short-lived cookie, which blocks login CSRF), exchanges the code, asks X `/2/users/me` who signed in, and creates or updates the user `x-<X id>`. X's access token is used for that one lookup and not stored.
- **Sessions**: 30 days, random 256-bit tokens; only their SHA-256 is stored. Sent as an `HttpOnly; SameSite=Lax` cookie (`Secure` unless the callback is plain http), or as `Authorization: Bearer <token>` for API clients. `POST /auth/logout` ends it; `GET /api/me` returns the signed-in user.
- **Config**: `X_CLIENT_ID` (required to enable), `X_CLIENT_SECRET` (confidential clients), `X_REDIRECT_URI`; `X_AUTHORIZE_URL`, `X_TOKEN_URL`, `X_API_BASE` override X's endpoints. `AUTH_DEV_LOGIN=1` enables `POST /auth/dev/login {"user_id"}`, which signs in as anyone: local development only (the server warns at startup).
- **Python**: `trend_liker.auth.sign_in_with_x(approve)` runs the flow and returns a session token; pass `token=` to `ForUsClient` / `LobbyClient`.
- **Without an X app**: `python -m trend_liker.mock_x_oauth --port 9999` is a local stand-in for X's OAuth endpoints (auto-approves; `login_as=<handle>` picks who; PKCE enforced), then:

  ```
  X_CLIENT_ID=mock X_AUTHORIZE_URL=http://127.0.0.1:9999/i/oauth2/authorize \
  X_TOKEN_URL=http://127.0.0.1:9999/2/oauth2/token X_API_BASE=http://127.0.0.1:9999 cargo run
  ```

Cross-site request forgery: the session cookie is `SameSite=Lax`, state-changing routes need JSON bodies or non-simple methods, and the server sends no CORS headers, so other sites can't make those requests with your cookie. Pending sign-ins are held in memory, so a sign-in can't span a server restart or multiple instances.
