import json

import httpx
import pytest

from trend_liker.agent import FeedAgent, Policy
from trend_liker.finance import is_finance
from trend_liker.forus import ForUsError
from trend_liker.lobby import LobbyClient, share_feed
from trend_liker.mock_x import MockXServer, make_tweet
from trend_liker.models import Post, Profile
from trend_liker.x_client import HttpXClient


@pytest.mark.parametrize("text,expected", [
    ("Loading up on $NVDA before earnings", True),
    ("$tsla, to the moon", True),
    ("my stock portfolio", True),
    ("costs $5 per month", False),
    ("too many API calls", False),
    ("Rust and Cargo", False),
])
def test_is_finance_matches_server_cases(text, expected):
    assert is_finance(text) is expected


async def test_agent_never_auto_likes_finance_by_default():
    s = MockXServer({"u1": [make_tweet(1, "AI agents are great"), make_tweet(2, "AI picks: buy $NVDA")]})
    x = HttpXClient("test-token", transport=s.transport)
    agent = FeedAgent(x, Profile("u1", frozenset({"ai"})), Policy(dry_run=False))
    r = await agent.run()
    assert r.liked == ["1"] and r.skipped_finance == 1 and s.likes["u1"] == {"1"}
    opted = FeedAgent(x, Profile("u1", frozenset({"ai"})), Policy(dry_run=False, like_finance=True))
    assert len((await opted.run()).liked) == 2


def recorder():
    seen = []

    def h(req):
        body = json.loads(req.content) if req.content else None
        seen.append((req.method, req.url.path, dict(req.url.params), body))
        if req.url.path.endswith("/contribute"):
            return httpx.Response(200, json={"added": len(body["items"]), "merged": 0, "off_topic": 0, "rejected": 0, "evicted": 0})
        if req.url.path.endswith("/interactions"):
            return httpx.Response(200, json={"version": 7})
        return httpx.Response(200, json={})

    return seen, LobbyClient(transport=httpx.MockTransport(h))


async def test_request_shapes():
    seen, c = recorder()
    await c.create("ai", "AI", "demo", {"affinity": 0.8})
    await c.join("ai", "bob")
    assert await c.interact("ai", "bob", "item-1", "like") == 7
    await c.interact("ai", "bob", "item-1", "like", active=False)
    await c.feed("ai", "bob", limit=10)
    await c.leave("ai", "bob")
    assert seen == [
        ("POST", "/api/lobbies", {}, {"id": "ai", "name": "AI", "owner": "demo", "policy": {"affinity": 0.8}}),
        ("POST", "/api/lobbies/ai/members", {}, {"user_id": "bob"}),
        ("POST", "/api/lobbies/ai/interactions", {}, {"user_id": "bob", "item_id": "item-1", "kind": "like", "active": True}),
        ("POST", "/api/lobbies/ai/interactions", {}, {"user_id": "bob", "item_id": "item-1", "kind": "like", "active": False}),
        ("GET", "/api/lobbies/ai/feed", {"user": "bob", "limit": "10"}, None),
        ("DELETE", "/api/lobbies/ai/members/bob", {}, None),
    ]
    with pytest.raises(ValueError):
        await c.interact("ai", "bob", "item-1", "upvote")


async def test_share_feed_contributes_own_timeline_minus_protected():
    seen, c = recorder()
    s = MockXServer({"bob": [make_tweet(1, "rust", "pub"), make_tweet(2, "secret", "priv")]})
    s.protected_authors = {"priv"}
    agent = FeedAgent(HttpXClient("test-token", transport=s.transport), Profile("bob"))
    totals = await share_feed(c, agent, "ai")
    assert totals["added"] == 1 and totals["skipped_protected"] == 1
    method, path, _, body = seen[-1]
    assert (method, path, body["user_id"]) == ("POST", "/api/lobbies/ai/contribute", "bob")
    assert [i["external_id"] for i in body["items"]] == ["1"]


async def test_events_parses_sse_stream():
    stream = (
        "event: hello\ndata: {\"lobby\":\"ai\",\"version\":3}\n\n"
        ":\n\n"
        "event: interaction\ndata: {\"lobby\":\"ai\",\"version\":4,\"kind\":\"interaction\",\"item\":\"x\"}\n\n"
    )
    c = LobbyClient(transport=httpx.MockTransport(
        lambda req: httpx.Response(200, headers={"content-type": "text/event-stream"}, content=stream.encode())))
    got = [e async for e in c.events("ai", "bob")]
    assert got == [("hello", {"lobby": "ai", "version": 3}),
                   ("interaction", {"lobby": "ai", "version": 4, "kind": "interaction", "item": "x"})]


async def test_events_requires_membership():
    c = LobbyClient(transport=httpx.MockTransport(lambda req: httpx.Response(403, text="")))
    with pytest.raises(ForUsError) as e:
        async for _ in c.events("ai", "stranger"):
            pass
    assert e.value.status == 403
