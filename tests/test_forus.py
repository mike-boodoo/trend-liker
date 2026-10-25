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
