"""trend-liker as an MCP connector: lets Claude read and act in the For Us / lobby service.

    python -m trend_liker.mcp_server                       # stdio (Claude Code, Claude Desktop)
    python -m trend_liker.mcp_server --http --port 8765    # streamable HTTP on 127.0.0.1

The connector acts as one signed-in user of the universal-feed server:

    TRENDLIKER_URL       server URL (default http://127.0.0.1:3000)
    TRENDLIKER_TOKEN     a session token from Sign in with X (see trend_liker.auth), or
    TRENDLIKER_DEV_USER  a test account to sign in as via dev sign-in (server needs AUTH_DEV_LOGIN=1);
                         also enables the run_simulation tool
"""
from __future__ import annotations

import argparse
import asyncio
import logging
import os
from typing import Any, Literal

import httpx
from mcp.server.mcpserver import MCPServer
from mcp.server.mcpserver.exceptions import ToolError
from mcp.types import ToolAnnotations

from .auth import dev_login
from .forus import DEFAULT_URL, ForUsClient, ForUsError
from .lobby import LobbyClient
from .models import Post

logging.getLogger("httpx").setLevel(logging.WARNING)  # one INFO line per request is noise here

INSTRUCTIONS = """\
trend-liker hosts shared feeds. "For Us" public feeds are topic feeds a user subscribes to; \
lobbies are shared pools where members contribute posts from their own feeds and every vote \
(like, bookmark, reply, skip) re-ranks the pool for every other member. All actions are taken \
as the signed-in user shown by `whoami`.

Votes are visible to other members as ranking changes, so vote only when the user asks you to. \
Contributing posts or turning publishing on shares the user's content with others; turning \
publishing off withdraws everything they contributed. Confirm those with the user first. \
Finance posts are never amplified by votes unless the lobby's owner enabled it."""

READ = ToolAnnotations(read_only_hint=True, destructive_hint=False, idempotent_hint=True, open_world_hint=False)


def _write(destructive: bool = False, idempotent: bool = False) -> ToolAnnotations:
    return ToolAnnotations(read_only_hint=False, destructive_hint=destructive, idempotent_hint=idempotent,
                           open_world_hint=False)


def _explain(e: ForUsError) -> ToolError:
    hint = {
        401: "not signed in: set TRENDLIKER_TOKEN (a session token from Sign in with X) or TRENDLIKER_DEV_USER",
        403: "not allowed (not a member, not the owner, or publishing is off for this user)",
        404: "not found",
        409: "conflict (already exists, lobby full, or the owner can't leave)",
        413: "too many items in one request",
    }.get(e.status, "request failed")
    detail = str(e).split(": ", 1)[-1].strip()
    return ToolError(f"{hint} [{e.status}]" + (f": {detail}" if detail and e.status == 422 else ""))


def _item(i: dict) -> dict:
    """The fields Claude needs from a ranked lobby post."""
    return {k: i[k] for k in ("id", "text", "author", "categories", "score", "likes", "bookmarks",
                              "contributors", "seen", "mine", "explore", "amplified") if k in i}


def build_server(base_url: str = DEFAULT_URL, token: str | None = None, dev_user: str | None = None,
                 transport: httpx.AsyncBaseTransport | None = None) -> MCPServer:
    server = MCPServer(name="trend-liker", title="trend-liker feeds and lobbies", instructions=INSTRUCTIONS,
                       version="0.1.0")
    state: dict[str, Any] = {"token": token}

    async def session_token() -> str | None:
        if state["token"] is None and dev_user:
            state["token"] = await dev_login(dev_user, base_url, transport)
        return state["token"]

    async def call(fn):
        """Run an API call with the connector's session, turning service errors into tool errors."""
        tok = await session_token()
        try:
            return await fn(ForUsClient(base_url, transport, tok), LobbyClient(base_url, transport, tok))
        except ForUsError as e:
            raise _explain(e) from e
        except httpx.HTTPError as e:
            raise ToolError(f"can't reach the trend-liker server at {base_url}: {e}") from e

    async def me(fu: ForUsClient) -> dict:
        r = await fu._http.get("/api/me")
        if r.status_code >= 400:
            raise ForUsError(r.status_code, r.text)
        return r.json()

    @server.tool(annotations=READ)
    async def whoami() -> dict:
        """The signed-in user: id, X handle, subscribed public feeds, and whether publishing is on."""
        return await call(lambda fu, lc: me(fu))

    @server.tool(annotations=READ)
    async def list_public_feeds() -> dict:
        """The "For Us" public topic feeds (id, name, how many posts)."""
        feeds = await call(lambda fu, lc: fu.feeds())
        return {"feeds": [{"id": f["id"], "name": f["name"], "posts": len(f["items"])} for f in feeds]}

    @server.tool(annotations=READ)
    async def my_home_feed(limit: int = 20) -> dict:
        """The user's "For Me" home feed: posts from their subscribed public feeds, best first."""
        async def go(fu, lc):
            return await fu.home_feed((await me(fu))["user_id"])
        items = await call(go)
        return {"items": [{k: i[k] for k in ("id", "source", "author", "text", "categories", "score", "url")}
                          for i in items[: max(1, min(limit, 200))]]}

    @server.tool(annotations=_write(idempotent=True))
    async def subscribe_feed(feed: str) -> dict:
        """Subscribe the user to a public feed by id (see list_public_feeds). Safe to repeat."""
        async def go(fu, lc):
            return await fu.subscribe((await me(fu))["user_id"], feed)
        return {"public_feeds": await call(go)}

    @server.tool(annotations=_write(destructive=True, idempotent=True))
    async def set_publishing(enabled: bool) -> dict:
        """Turn on or off whether the user's posts may be shared into public feeds and lobbies.
        Turning it off withdraws every post they contributed (except ones another opted-in member
        also contributed). Confirm with the user before changing it."""
        async def go(fu, lc):
            return await fu.set_publishing((await me(fu))["user_id"], enabled)
        return await call(go)

    @server.tool(annotations=READ)
    async def list_lobbies() -> dict:
        """All lobbies: id, name, owner, member count and version."""
        async def go(fu, lc):
            r = await lc._http.get("/api/lobbies")
            r.raise_for_status()
            return r.json()
        return {"lobbies": [{k: l[k] for k in ("id", "name", "owner", "members", "version")} for l in await call(go)]}

    @server.tool(annotations=READ)
    async def lobby_feed(lobby: str, limit: int = 20) -> dict:
        """A lobby's pool ranked for the user (they must be a member). Each post has its score, the
        user's own votes (`mine`), whether it's an exploration pick, and whether finance rules kept
        it from being amplified. `locked` lists labels the user can't see yet and their progress."""
        async def go(fu, lc):
            return await lc.feed(lobby, (await me(fu))["user_id"], limit=max(1, min(limit, 200)))
        f = await call(go)
        return {"lobby": f["lobby"], "version": f["version"], "items": [_item(i) for i in f["items"]],
                "locked": f.get("locked", [])}

    @server.tool(annotations=_write())
    async def create_lobby(lobby: str, name: str, policy: dict | None = None) -> dict:
        """Create a lobby owned by the user. `lobby` is its id (lowercase letters, digits, dashes).
        `policy` optionally sets ranking (e.g. {"affinity": 0.9}) and access rules
        (e.g. {"access": [{"label": "search", "when": {"produced": 1}}]})."""
        return await call(lambda fu, lc: lc.create(lobby, name, None, policy))  # type: ignore[arg-type]

    @server.tool(annotations=_write(idempotent=True))
    async def join_lobby(lobby: str) -> dict:
        """Join a lobby as the user. Safe to repeat."""
        async def go(fu, lc):
            r = await lc._http.post(f"/api/lobbies/{lobby}/members", json={})
            if r.status_code >= 400:
                raise ForUsError(r.status_code, r.text)
            return r.json()
        return await call(go)

    @server.tool(annotations=_write(destructive=True))
    async def leave_lobby(lobby: str) -> dict:
        """Leave a lobby. Removes the user's votes and withdraws posts only they contributed."""
        async def go(fu, lc):
            return await lc.leave(lobby, (await me(fu))["user_id"])
        return await call(go)

    @server.tool(annotations=_write(idempotent=True))
    async def vote(lobby: str, item_id: str,
                   kind: Literal["like", "bookmark", "reply", "skip", "impression"] = "like",
                   active: bool = True) -> dict:
        """Vote on a lobby post as the user (one vote per kind; repeating changes nothing).
        `active: false` retracts it. Every member's ranking changes, so only vote when asked."""
        async def go(fu, lc):
            r = await lc._http.post(f"/api/lobbies/{lobby}/interactions",
                                    json={"item_id": item_id, "kind": kind, "active": active})
            if r.status_code >= 400:
                raise ForUsError(r.status_code, r.text)
            return r.json()
        return await call(go)

    @server.tool(annotations=_write())
    async def contribute_posts(lobby: str, posts: list[dict]) -> dict:
        """Share posts into a lobby as coming from the user's feed. Each post needs `id` and `text`;
        `author` and `like_count` are optional. Use posts the user saw elsewhere (e.g. from another
        connector) with their permission. Requires publishing to be on."""
        try:
            parsed = [Post(id=str(p["id"]), author_id=str(p.get("author", "")), text=str(p["text"]),
                           like_count=int(p.get("like_count", 0))) for p in posts]
        except (KeyError, TypeError, ValueError) as e:
            raise ToolError(f"each post needs `id` and `text` ({e})") from e

        async def go(fu, lc):
            return await lc.contribute(lobby, (await me(fu))["user_id"], parsed)
        return await call(go)

    @server.tool(annotations=READ)
    async def classify_text(texts: list[str]) -> dict:
        """The labels the service would give each text (e.g. ai, rust, finance), as used for feeds,
        lobby topics and access rules."""
        async def go(fu, lc):
            r = await fu._http.post("/api/classify", json={"texts": texts[:500]})
            r.raise_for_status()
            return r.json()["results"]
        return {"results": await call(go)}

    if dev_user:
        @server.tool(annotations=_write())
        async def run_simulation(rounds: int = 3, others: int = 2, policy: dict | None = None) -> dict:
            """Dev only: split a mock feed into category test accounts, add other test accounts,
            run voting rounds in a new lobby, and report how the ranking moved for the user."""
            import json as _json
            import tempfile

            from . import devsim
            args = devsim.parse_args(["--server", base_url, "--you", dev_user, "--rounds", str(rounds),
                                      "--others", str(others), "--report", ""])
            if policy:
                with tempfile.NamedTemporaryFile("w", suffix=".json", delete=False) as f:
                    _json.dump(policy, f)
                args.policy = f.name
            try:
                r = await devsim.run(args, transport)
            except ForUsError as e:
                raise _explain(e) from e
            after = r.rounds[-1][1] if r.rounds else r.before
            return {
                "lobby": r.lobby,
                "slices": {label: len(p) for label, p in r.slices.items()},
                "votes": [v for v, _ in r.rounds],
                "your_view": [{"text": i["text"], "move": devsim.arrow(d), "score": round(i["score"], 2),
                               "categories": i["categories"]} for i, d in devsim.movement(r.before, after)],
                "watch": f"{base_url}/lobbies/{r.lobby}",
            }

    return server


def main(argv: list[str] | None = None) -> None:
    ap = argparse.ArgumentParser(prog="python -m trend_liker.mcp_server", description=__doc__.splitlines()[0])
    ap.add_argument("--http", action="store_true", help="serve streamable HTTP instead of stdio")
    ap.add_argument("--port", type=int, default=8765)
    a = ap.parse_args(argv)
    server = build_server(os.environ.get("TRENDLIKER_URL", DEFAULT_URL), os.environ.get("TRENDLIKER_TOKEN") or None,
                          os.environ.get("TRENDLIKER_DEV_USER") or None)
    if a.http:
        # Localhost only: whoever can reach it acts as the connector's user.
        asyncio.run(server.run_streamable_http_async(host="127.0.0.1", port=a.port))
    else:
        server.run("stdio")


if __name__ == "__main__":
    main()
