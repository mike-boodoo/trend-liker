#!/usr/bin/env bash
# Dev testing flow: start the server in dev mode (dev sign-in on, in-memory storage unless
# DEV_DATABASE_URL is set), run the feed simulation, and keep the server up for the live pages.
# Extra arguments go to the simulation, e.g.  ./scripts/dev.sh --rounds 5 --others 3
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
url=http://127.0.0.1:3000

if curl -sf -o /dev/null "$url/api/feeds"; then
  echo "Something is already serving $url; stop it first." >&2
  exit 1
fi
(cd "$root/universal-feed" && cargo build -q)
AUTH_DEV_LOGIN=1 DATABASE_URL="${DEV_DATABASE_URL:-}" "$root/universal-feed/target/debug/universal-feed" &
server=$!
trap 'kill "$server" 2>/dev/null || true' EXIT
for _ in $(seq 100); do curl -sf -o /dev/null "$url/api/feeds" && break; sleep 0.1; done

PYTHONPATH="$root${PYTHONPATH:+:$PYTHONPATH}" python3 -m trend_liker.devsim --server "$url" "$@"
echo "Server still running at $url (Ctrl-C to stop)."
wait "$server"
