import json

import httpx

from mcp import Client
from trend_liker.mcp_server import build_server


class FakeService:
    """Just enough of the universal-feed API, enforcing the session token."""

    def __init__(self):
        self.calls = []

    def __call__(self, req: httpx.Request) -> httpx.Response:
        body = json.loads(req.content) if req.content else None
        self.calls.append((req.method, req.url.path, dict(req.url.params), body))
        if req.url.path == "/auth/dev/login":
            return httpx.Response(200, json={"user_id": body["user_id"], "token": "devtok"})
        if req.headers.get("authorization") not in ("Bearer tok", "Bearer devtok"):
            return httpx.Response(401)
        p = req.url.path
        if p == "/api/me":
            return httpx.Response(200, json={"user_id": "x-42", "username": "mike", "public_feeds": ["ai"], "publish_opt_in": False})
        if p == "/api/lobbies/crew/feed":
            if req.url.params.get("user") != "x-42":
                return httpx.Response(403)
            return httpx.Response(200, json={"lobby": "crew", "version": 7, "locked": [], "items": [
                {"id": "i1", "text": "rust", "author": "a", "categories": ["rust"], "score": 1.5, "likes": 2, "bookmarks": 0,
                 "contributors": 1, "seen": False, "mine": [], "explore": False, "amplified": True, "global": 1, "personal": 0}]})
        if p == "/api/lobbies/crew/interactions":
            return httpx.Response(200, json={"version": 8})
        if p == "/api/lobbies/crew/contribute":
            return httpx.Response(403)  # publishing is off
        if p == "/api/classify":
            return httpx.Response(200, json={"results": [{"labels": ["finance"], "finance": True}] * len(body["texts"])})
        return httpx.Response(404)


def text(result) -> object:
    return json.loads(result.content[0].text)


async def test_tools_and_annotations():
    async with Client(build_server("http://s", "tok", transport=httpx.MockTransport(FakeService()))) as c:
        tools = {t.name: t for t in (await c.list_tools()).tools}
    assert "run_simulation" not in tools, "dev-only tool hidden unless TRENDLIKER_DEV_USER is set"
    for name in ("whoami", "lobby_feed", "list_lobbies", "classify_text"):
        assert tools[name].annotations.read_only_hint
    for name in ("set_publishing", "leave_lobby"):
        assert tools[name].annotations.destructive_hint and not tools[name].annotations.read_only_hint
    assert not tools["vote"].annotations.read_only_hint
    assert "Votes are visible" in (await _instructions())


async def _instructions():
    async with Client(build_server("http://s", "tok", transport=httpx.MockTransport(FakeService()))) as c:
        return c.instructions or ""


async def test_acts_as_signed_in_user():
    svc = FakeService()
    async with Client(build_server("http://s", "tok", transport=httpx.MockTransport(svc))) as c:
        assert text(await c.call_tool("whoami", {}))["user_id"] == "x-42"
        feed = text(await c.call_tool("lobby_feed", {"lobby": "crew", "limit": 5}))
        assert feed["version"] == 7 and feed["items"][0] == {
            "id": "i1", "text": "rust", "author": "a", "categories": ["rust"], "score": 1.5, "likes": 2, "bookmarks": 0,
            "contributors": 1, "seen": False, "mine": [], "explore": False, "amplified": True}
        assert text(await c.call_tool("vote", {"lobby": "crew", "item_id": "i1", "kind": "bookmark"}))["version"] == 8
    vote = [x for x in svc.calls if x[1].endswith("/interactions")][0]
    assert vote[3] == {"item_id": "i1", "kind": "bookmark", "active": True}, "no user_id sent: the session decides"


async def test_errors_are_readable():
    async with Client(build_server("http://s", None, transport=httpx.MockTransport(FakeService()))) as c:
        r = await c.call_tool("whoami", {})
        assert r.is_error and "not signed in" in r.content[0].text
    async with Client(build_server("http://s", "tok", transport=httpx.MockTransport(FakeService()))) as c:
        r = await c.call_tool("contribute_posts", {"lobby": "crew", "posts": [{"id": "1", "text": "hi"}]})
        assert r.is_error and "publishing is off" in r.content[0].text
        r = await c.call_tool("contribute_posts", {"lobby": "crew", "posts": [{"text": "no id"}]})
        assert r.is_error and "needs `id` and `text`" in r.content[0].text
        r = await c.call_tool("vote", {"lobby": "crew", "item_id": "i1", "kind": "upvote"})
        assert r.is_error, "kind is validated"
        r = await c.call_tool("join_lobby", {"lobby": "nope"})
        assert r.is_error and "[404]" in r.content[0].text
        r = await c.call_tool("classify_text", {"texts": ["a", "b"]})
        assert len(r.content) == 1 and len(text(r)["results"]) == 2, "one object per result, not one block per item"


async def test_dev_user_signs_in_lazily_and_enables_simulation():
    svc = FakeService()
    async with Client(build_server("http://s", dev_user="dev-you", transport=httpx.MockTransport(svc))) as c:
        assert "run_simulation" in {t.name for t in (await c.list_tools()).tools}
        assert not any(x[1] == "/auth/dev/login" for x in svc.calls), "no sign-in until a tool needs it"
        assert text(await c.call_tool("whoami", {}))["username"] == "mike"
        await c.call_tool("whoami", {})
    assert sum(x[1] == "/auth/dev/login" for x in svc.calls) == 1, "signs in once"
