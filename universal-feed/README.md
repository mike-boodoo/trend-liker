# Universal Feed — Local For-Us Feed Server

A small Rust/Axum prototype for a **For Us / For Me** feed architecture.

## Concept

- **FOR US** = a shared public feed/category.
- **FOR ME** = private settings describing which public feeds a user subscribes to.
- One canonical content item can belong to multiple public feeds.
- The home feed merges subscribed public feeds and removes duplicates.
- X and TikTok are candidate sources rather than authorities over your final feed order.

## Architecture

```text
                 LOCAL SERVER
                      |
            +---------+---------+
            |                   |
       PUBLIC FEEDS         USER SETTINGS
            |                   |
      +-----+------+            |
      |     |      |            |
     AI    Rust   Cloud          |
      |     |      |            |
      +-----+------+            |
            |                   |
         FOR-US          FOR-ME subscriptions
            |                   |
            +---------+---------+
                      |
                   HOME FEED
```

## Requirements

Install Rust and Cargo from https://www.rust-lang.org/tools/install

Verify:

```bash
rustc --version
cargo --version
```

## Run

From the project directory:

```bash
cargo run
```

Open:

```text
http://localhost:3000
```

No API keys are required for the demo.

## Demo data

The server seeds public feeds including AI, Rust, Cloud, Programming, Infrastructure, Agents, Search, and Kubernetes.

The demo user is `demo`, initially subscribed to:

```text
AI
Rust
Cloud
```

## API

List public feeds:

```bash
curl http://localhost:3000/api/feeds
```

Get subscriptions:

```bash
curl http://localhost:3000/api/users/demo/subscriptions
```

Get the home feed:

```bash
curl http://localhost:3000/api/users/demo/feed
```

Subscribe to another public feed:

```bash
curl -X POST http://localhost:3000/api/users/demo/subscriptions \
  -H 'Content-Type: application/json' \
  -d '{"feed":"programming"}'
```

Then:

```bash
curl http://localhost:3000/api/users/demo/feed
```

## Source adapters

The repository contains:

```text
src/adapters/x.rs
src/adapters/tiktok.rs
```

They are intentionally placeholders. The intended production flow is:

```text
X API -----------+
TikTok API ------+
Reddit API ------+--> normalize candidates --> classify --> FOR-US feed
GitHub API ------+
RSS/News --------+
```

The available API permissions and endpoints determine what third-party content can actually be retrieved. This prototype does not attempt to reproduce a platform's private consumer ranking algorithm.

## Environment

Optional:

```bash
cp .env.example .env
```

Then add credentials if you later implement the adapters:

```text
X_BEARER_TOKEN=
TIKTOK_ACCESS_TOKEN=
```

## Development

Format:

```bash
cargo fmt
```

Check compilation:

```bash
cargo check
```

Run tests when added:

```bash
cargo test
```

## Next stage: FeedQL

The intended query language can look like:

```text
FOR US "AI"

FROM X, TIKTOK, REDDIT, GITHUB

WHERE topic ~= "AI"

CLASSIFY INTO
    agents
    models
    infrastructure
    research

RANK BY
    relevance
    recency
    novelty

PUBLISH
    TO "AI"
```

The eventual runtime would be:

```text
FeedQL
  |
  v
Parser / AST
  |
  v
Candidate retrieval
  |
  v
Classification
  |
  v
Ranking
  |
  v
Public Feed Snapshot
  |
  v
For-Me subscription merger
  |
  v
Browser / mobile client
```

## Persistence roadmap

The current implementation is intentionally in-memory. A production version should move to PostgreSQL tables such as:

```text
users
public_feeds
content_items
feed_items
user_subscriptions
categories
content_categories
feed_snapshots
```

Feed snapshots can make a public feed reproducible:

```text
AI
  snapshot 1042
  snapshot 1043
  snapshot 1044
```

## Project status

This is a runnable local prototype demonstrating the core abstraction:

> **FOR US defines the shared feed; FOR ME defines which shared feeds I receive.**
