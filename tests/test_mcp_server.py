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


# --- hosted mode and X feed analysis ---
from trend_liker.mcp_server import ServiceTokenVerifier, analyze


def test_analyze_feed_patterns():
    posts = ([{"id": str(i), "text": f"rust post {i}", "author_username": "rusty", "like_count": i, "created_at": f"2026-10-2{i % 3}T00:00:00Z"} for i in range(6)]
             + [{"id": "a", "text": "$NVDA to the moon", "author_username": "pump", "like_count": 900, "protected": False},
                {"id": "b", "text": "coffee", "author_username": "pal", "like_count": 1, "protected": True}])
    labels = [["rust"]] * 6 + [["finance"], []]
    a = analyze(posts, labels)
    assert (a["posts"], a["authors"], a["protected_posts"]) == (8, 3, 1)
    assert a["topics"][0] == {"label": "rust", "posts": 6, "share": 0.75}
    assert a["finance_share"] == 0.125 and a["unlabeled_share"] == 0.125
    assert a["top_authors"][0] == {"author": "rusty", "posts": 6, "mostly": "rust"}
    assert a["concentration"]["top5_authors_share"] == 1.0
    assert a["engagement"]["most_liked"][0]["author"] == "pump"
    assert a["as_lobby_slices"] == {"rust": 6, "finance": 1, "unlabeled": 1}
    assert a["time_span"] == {"oldest": "2026-10-20T00:00:00Z", "newest": "2026-10-22T00:00:00Z"}
    assert len(a["sample"]) == 8
    assert analyze([], [])["posts"] == 0


async def test_token_verifier_checks_service_and_caches():
    seen = []

    def h(req):
        seen.append(req.headers.get("authorization"))
        ok = req.headers.get("authorization") == "Bearer good"
        return httpx.Response(200, json={"user_id": "x-42"}) if ok else httpx.Response(401)

    v = ServiceTokenVerifier("http://s", "https://mcp.example/mcp", httpx.MockTransport(h))
    t = await v.verify_token("good")
    assert (t.subject, t.resource, t.token) == ("x-42", "https://mcp.example/mcp", "good")
    await v.verify_token("good")
    assert await v.verify_token("bad") is None and await v.verify_token("bad") is None
    assert seen == ["Bearer good", "Bearer bad"], "one lookup per token within the cache window"


async def test_x_tools_explain_missing_connection_and_rate_limits():
    def h(req):
        if req.url.path == "/api/me/x/timeline":
            if req.headers.get("x-case") == "limit":
                return httpx.Response(429, headers={"retry-after": "60"})
            return httpx.Response(409, json={"error": "x_not_connected", "detail": "no X connection stored"})
        return FakeService()(req)

    async with Client(build_server("http://s", "tok", transport=httpx.MockTransport(h))) as c:
        r = await c.call_tool("analyze_my_feed", {})
        assert r.is_error and "sign in with X again" in r.content[0].text
    tools = {t.name: t for t in (await _tools())}
    assert tools["my_x_timeline"].annotations.read_only_hint and tools["disconnect_x"].annotations.destructive_hint


async def _tools():
    async with Client(build_server("http://s", "tok", transport=httpx.MockTransport(FakeService()))) as c:
        return (await c.list_tools()).tools


async def test_hosted_mode_ignores_local_credentials():
    s = build_server("http://s", token="tok", dev_user="dev-you", public_url="https://mcp.example")
    async with Client(s) as c:
        names = {t.name for t in (await c.list_tools()).tools}
        assert "run_simulation" not in names and "analyze_my_feed" in names, "no dev tools on a hosted connector"
        r = await c.call_tool("whoami", {})
        assert r.is_error and "no signed-in caller" in r.content[0].text, "never falls back to a configured token"


async def test_hosted_metadata_points_at_public_issuer():
    import httpx2
    s = build_server("http://127.0.0.1:3000", public_url="https://feeds.example.com", issuer_url="https://feeds.example.com")
    app = s.streamable_http_app(transport_security=None)
    async with httpx2.AsyncClient(transport=httpx2.ASGITransport(app=app), base_url="https://feeds.example.com") as c:
        meta = (await c.get("/.well-known/oauth-protected-resource/mcp")).json()
        assert meta["resource"] == "https://feeds.example.com/mcp"
        assert meta["authorization_servers"] == ["https://feeds.example.com"], "public issuer (matching the server's issuer exactly), not the internal URL"
        r = await c.post("/mcp", json={})
        assert r.status_code == 401 and "resource_metadata=" in r.headers["www-authenticate"]
