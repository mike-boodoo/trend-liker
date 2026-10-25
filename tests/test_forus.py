import json

import httpx
import pytest

from trend_liker.connector import Query, Registry
from trend_liker.forus import ForUsClient, ForUsConnector, ForUsError


def item(i, text, cats, score):
    return {"id": i, "source": "Local", "author": "x", "text": text, "url": None,
            "categories": cats, "created_at": "2026-10-25T00:00:00Z", "score": score}


def handler(req: httpx.Request) -> httpx.Response:
    p = req.url.path
    if p == "/api/users/demo/feed":
        return httpx.Response(200, json={"user_id": "demo", "feeds": ["ai"], "items": [
            item("1", "AI inference infrastructure", ["ai", "cloud"], 0.98),
            item("2", "Building Rust services", ["rust"], 0.93)]})
    if p == "/api/users/demo/subscriptions" and req.method == "POST":
        assert json.loads(req.content) == {"feed": "rust"}
        return httpx.Response(200, json={"user_id": "demo", "public_feeds": ["ai", "rust"]})
    if p == "/api/users/demo/subscriptions":
        return httpx.Response(200, json={"user_id": "demo", "public_feeds": ["ai"]})
    return httpx.Response(404)


def client():
    return ForUsClient(transport=httpx.MockTransport(handler))


async def test_client_roundtrip():
    c = client()
    assert await c.subscriptions("demo") == ["ai"]
    assert await c.subscribe("demo", "rust") == ["ai", "rust"]
    assert len(await c.home_feed("demo")) == 2


async def test_unknown_user_raises():
    with pytest.raises(ForUsError) as e:
        await client().home_feed("nobody")
    assert e.value.status == 404


async def test_connector_filters_by_term_and_category():
    reg = Registry()
    reg.register(ForUsConnector(client()))
    res = await reg.query(Query("demo", frozenset({"rust", "cloud"})))
    assert [r.id for r in res.records] == ["1", "2"] and not res.errors
    res = await reg.query(Query("demo", frozenset({"rust"})))
    assert [r.id for r in res.records] == ["2"]


async def test_connector_error_isolated_in_registry():
    reg = Registry()
    reg.register(ForUsConnector(client()))
    res = await reg.query(Query("nobody", frozenset({"ai"})))
    assert res.records == [] and "ForUsError" in res.errors["for-us"]


# --- ingest / publish ---
from trend_liker.forus import MAX_BATCH, publish_posts, to_candidate
from trend_liker.mock_x import MockXServer, make_tweet
from trend_liker.models import Post
from trend_liker.x_client import HttpXClient


def test_to_candidate_scores_and_shape():
    c = to_candidate(Post("7", "a1", "hi rust", like_count=5_000))
    assert c["source"] == "X" and c["external_id"] == "7" and c["score"] == 0.75
    assert c["url"].endswith("/7")
    assert to_candidate(Post("8", "a", "t", like_count=10**9))["score"] == 1.0


async def test_publish_skips_protected_and_batches():
    sent = []

    def h(req):
        body = json.loads(req.content)
        sent.append(body["items"])
        return httpx.Response(200, json={"ingested": [{}] * len(body["items"]),
                                         "duplicates": 0, "unclassified": 0, "rejected": 0})

    c = ForUsClient(transport=httpx.MockTransport(h))
    posts = [Post(str(i), "a", "rust") for i in range(MAX_BATCH + 1)]
    posts.append(Post("p", "priv", "secret rust", protected=True))
    totals = await publish_posts(c, posts)
    assert totals["ingested"] == MAX_BATCH + 1 and totals["skipped_protected"] == 1
    assert [len(b) for b in sent] == [MAX_BATCH, 1]
    assert all(i["external_id"] != "p" for b in sent for i in b)


async def test_end_to_end_x_mock_to_ingest_payload():
    s = MockXServer({"u1": [make_tweet(1, "Rust news", author="pub"), make_tweet(2, "Rust secret", author="priv")]})
    s.protected_authors = {"priv"}
    x = HttpXClient("test-token", transport=s.transport)
    page = await x.home_timeline("u1")
    assert {p.id: p.protected for p in page.posts} == {"1": False, "2": True}
    assert "protected" in s.requests[0].url.params["user.fields"]
