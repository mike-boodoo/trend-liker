"""trend-liker as an MCP connector: lets Claude read and act in the For Us / lobby service.

    python -m trend_liker.mcp_server                       # stdio (Claude Code, Claude Desktop)
    python -m trend_liker.mcp_server --http --port 8765    # streamable HTTP on 127.0.0.1
    python -m trend_liker.mcp_server --http --public-url https://mcp.example.com
                                                           # hosted connector (e.g. claude.ai): each
                                                           # person signs in with X via OAuth

Hosted mode: callers bring an OAuth access token issued by the universal-feed server, and each
call acts as that person. TRENDLIKER_ISSUER is that server's public URL, which clients are sent
to for sign-in (default: TRENDLIKER_URL, which may be an internal address behind a proxy).

Local modes act as one signed-in user of the universal-feed server:

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
import hashlib
import statistics
import time
from collections import Counter
from urllib.parse import urlparse

from mcp.server.auth.middleware.auth_context import get_access_token
from mcp.server.auth.provider import AccessToken, TokenVerifier
from mcp.server.auth.settings import AuthSettings
from mcp.server.mcpserver import MCPServer
from mcp.server.mcpserver.exceptions import ToolError
from mcp.server.transport_security import TransportSecuritySettings
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
Finance posts are never amplified by votes unless the lobby's owner enabled it.

`analyze_my_feed` and `my_x_timeline` read the user's own X home timeline (what X is showing \
them). That is private to the user: summarize it for them, and never contribute or publish it \
unless they explicitly ask."""

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


class ServiceTokenVerifier(TokenVerifier):
    """Accepts a bearer token if the universal-feed server says it's a live session. Results are
    cached briefly (by token hash) so each MCP request doesn't cost an extra round trip."""

    def __init__(self, base_url: str, resource: str, transport: httpx.AsyncBaseTransport | None = None,
                 ttl: float = 30.0):
        self.base_url, self.resource, self.transport, self.ttl = base_url, resource, transport, ttl
        self._cache: dict[str, tuple[float, str | None]] = {}

    async def verify_token(self, token: str) -> AccessToken | None:
        key = hashlib.sha256(token.encode()).hexdigest()
        hit = self._cache.get(key)
        if hit and hit[0] > time.monotonic():
            user = hit[1]
        else:
            async with httpx.AsyncClient(base_url=self.base_url, transport=self.transport, timeout=10.0) as c:
                r = await c.get("/api/me", headers={"Authorization": f"Bearer {token}"})
            user = r.json()["user_id"] if r.status_code == 200 else None
            if len(self._cache) > 10_000:
                self._cache.clear()
            self._cache[key] = (time.monotonic() + self.ttl, user)
        if user is None:
            return None
        return AccessToken(token=token, client_id="trend-liker", scopes=["feeds"], subject=user, resource=self.resource)


def analyze(posts: list[dict], labels: list[list[str]]) -> dict:
    """Patterns in a feed: what it's about, who dominates it, how concentrated it is, and what
    gets engagement. Plus an evenly spread sample for reading the posts themselves."""
    n = len(posts)
    if n == 0:
        return {"posts": 0, "note": "X returned no posts for this home timeline."}
    cats = Counter(l for ls in labels for l in ls)
    first = Counter((ls[0] if ls else "unlabeled") for ls in labels)
    authors = Counter(p.get("author_username") or p.get("author_id") or "?" for p in posts)
    by_author_labels: dict[str, Counter] = {}
    for p, ls in zip(posts, labels):
        by_author_labels.setdefault(p.get("author_username") or p.get("author_id") or "?", Counter()).update(ls or ["unlabeled"])
    likes = [int(p.get("like_count", 0)) for p in posts]
    times = sorted(t for t in (p.get("created_at") for p in posts) if t)
    top5 = sum(c for _, c in authors.most_common(5))
    step = max(1, n // 40)
    return {
        "posts": n,
        "authors": len(authors),
        "time_span": {"oldest": times[0], "newest": times[-1]} if times else None,
        "topics": [{"label": l, "posts": c, "share": round(c / n, 3)} for l, c in cats.most_common()],
        "unlabeled_share": round(sum(1 for ls in labels if not ls) / n, 3),
        "finance_share": round(sum(1 for ls in labels if "finance" in ls) / n, 3),
        "top_authors": [{"author": a, "posts": c, "mostly": by_author_labels[a].most_common(1)[0][0]}
                        for a, c in authors.most_common(10)],
        "concentration": {"top5_authors_share": round(top5 / n, 3),
                          "note": "share of the feed from its 5 most frequent accounts"},
        "engagement": {"median_likes": statistics.median(likes),
                       "most_liked": [{"text": p["text"][:200], "author": p.get("author_username"), "likes": p.get("like_count", 0),
                                       "url": p.get("url")} for p in sorted(posts, key=lambda p: -int(p.get("like_count", 0)))[:5]]},
        "as_lobby_slices": dict(first.most_common()),
        "protected_posts": sum(1 for p in posts if p.get("protected")),
        "sample": [{"text": posts[i]["text"][:280], "author": posts[i].get("author_username"), "labels": labels[i],
                    "likes": posts[i].get("like_count", 0)} for i in range(0, n, step)][:40],
    }


def build_server(base_url: str = DEFAULT_URL, token: str | None = None, dev_user: str | None = None,
                 transport: httpx.AsyncBaseTransport | None = None, public_url: str | None = None,
                 issuer_url: str | None = None) -> MCPServer:
    """`public_url` turns on hosted mode: OAuth with the universal-feed server as the
    authorization server, each request acting as its own caller (`token`/`dev_user` ignored)."""
    hosted = public_url is not None
    auth_kw: dict[str, Any] = {}
    if hosted:
        resource = public_url.rstrip("/") + "/mcp"
        auth_kw = {
            "token_verifier": ServiceTokenVerifier(base_url, resource, transport),
            # Tokens are this service's sessions (not bound to one MCP URL), so the verifier is the check.
            "auth": AuthSettings(issuer_url=issuer_url or base_url, resource_server_url=resource, required_scopes=None,
                                 validate_token_resource=False),
        }
        token, dev_user = None, None
    server = MCPServer(name="trend-liker", title="trend-liker feeds and lobbies", instructions=INSTRUCTIONS,
                       version="0.2.0", **auth_kw)
    state: dict[str, Any] = {"token": token}

    async def session_token() -> str | None:
        if hosted:
            caller = get_access_token()
            if caller is None:
                raise ToolError("no signed-in caller")
            return caller.token
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

    async def x_get(fu: ForUsClient, path: str, **params) -> dict:
        r = await fu._http.get(path, params=params, timeout=60.0)
        if r.status_code == 409:
            raise ToolError("X isn't connected for this account (or the connection expired): "
                            "sign in with X again, then retry. " + r.json().get("detail", ""))
        if r.status_code == 429:
            raise ToolError(f"X rate limit reached; try again in {r.headers.get('retry-after', 'a few')} seconds")
        if r.status_code >= 400:
            raise ForUsError(r.status_code, r.text)
        return r.json()

    @server.tool(annotations=READ)
    async def x_connection() -> dict:
        """Whether the user's X account is connected, so their home timeline can be read."""
        return await call(lambda fu, lc: x_get(fu, "/api/me/x"))

    @server.tool(annotations=ToolAnnotations(read_only_hint=True, destructive_hint=False, idempotent_hint=False,
                                             open_world_hint=True))
    async def my_x_timeline(limit: int = 50) -> dict:
        """The posts X is feeding the user right now (their home timeline, newest first, up to 800):
        text, author, time, likes/reposts/replies, link. Private to the user."""
        return await call(lambda fu, lc: x_get(fu, "/api/me/x/timeline", limit=max(1, min(limit, 800))))

    @server.tool(annotations=ToolAnnotations(read_only_hint=True, destructive_hint=False, idempotent_hint=False,
                                             open_world_hint=True))
    async def analyze_my_feed(limit: int = 200) -> dict:
        """Analyze the posts X is feeding the user: topics (the service's labels), which accounts
        dominate and how concentrated the feed is, what gets engagement, finance share, how a lobby
        would split it, and a spread-out sample of posts to read. Use it to explain the user's feed
        to them; it's private, so don't share it elsewhere unless they ask."""
        async def go(fu, lc):
            tl = await x_get(fu, "/api/me/x/timeline", limit=max(1, min(limit, 800)))
            posts = tl["posts"]
            labels: list[list[str]] = []
            for i in range(0, len(posts), 500):
                r = await fu._http.post("/api/classify", json={"texts": [p["text"] for p in posts[i : i + 500]]})
                r.raise_for_status()
                labels += [x["labels"] for x in r.json()["results"]]
            return analyze(posts, labels)
        return await call(go)

    @server.tool(annotations=_write(destructive=True, idempotent=True))
    async def disconnect_x() -> dict:
        """Forget the user's stored X connection (and revoke it at X). Timeline tools stop working
        until they sign in with X again. Confirm with the user first."""
        async def go(fu, lc):
            r = await fu._http.delete("/api/me/x")
            if r.status_code >= 400:
                raise ForUsError(r.status_code, r.text)
            return r.json()
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

    if dev_user and not hosted:
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
    ap.add_argument("--host", default="127.0.0.1", help="bind address (keep 127.0.0.1 behind a reverse proxy)")
    ap.add_argument("--public-url", help="hosted mode: this connector's public base URL (it serves <url>/mcp); "
                                         "callers sign in with OAuth")
    a = ap.parse_args(argv)
    if a.public_url and not a.http:
        ap.error("--public-url needs --http")
    server = build_server(os.environ.get("TRENDLIKER_URL", DEFAULT_URL), os.environ.get("TRENDLIKER_TOKEN") or None,
                          os.environ.get("TRENDLIKER_DEV_USER") or None, public_url=a.public_url,
                          issuer_url=os.environ.get("TRENDLIKER_ISSUER") or None)
    if not a.http:
        server.run("stdio")
        return
    hosts = [f"127.0.0.1:{a.port}", f"localhost:{a.port}"]
    origins = []
    if a.public_url:
        u = urlparse(a.public_url)
        hosts.append(u.netloc)
        origins.append(f"{u.scheme}://{u.netloc}")
    elif a.host != "127.0.0.1":
        ap.error("without --public-url (OAuth), serve on 127.0.0.1 only: anyone who can reach it acts as the connector's user")
    security = TransportSecuritySettings(enable_dns_rebinding_protection=True, allowed_hosts=hosts, allowed_origins=origins)
    asyncio.run(server.run_streamable_http_async(host=a.host, port=a.port, transport_security=security))


if __name__ == "__main__":
    main()
