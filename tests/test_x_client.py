import pytest

from trend_liker.mock_x import MockXServer, make_tweet
from trend_liker.x_client import HttpXClient, RateLimited, XError


def server(n=5):
    return MockXServer({"u1": [make_tweet(i, f"post {i}", likes=i) for i in range(n)]})


def client(s, **kw):
    return HttpXClient("test-token", transport=s.transport, **kw)


async def test_timeline_pagination_and_parsing():
    s = server(5)
    c = client(s)
    p1 = await c.home_timeline("u1", max_results=3)
    assert [p.id for p in p1.posts] == ["0", "1", "2"] and p1.next_token == "3"
    p2 = await c.home_timeline("u1", 3, p1.next_token)
    assert [p.id for p in p2.posts] == ["3", "4"] and p2.next_token is None
    assert p2.posts[1].like_count == 4
    req = s.requests[0]
    assert req.url.path == "/2/users/u1/timelines/reverse_chronological"
    assert req.headers["authorization"] == "Bearer test-token"


async def test_like_request_shape():
    s = server()
    assert await client(s).like("u1", "2") is True
    req = s.requests[-1]
    assert req.method == "POST" and req.url.path == "/2/users/u1/likes"
    assert s.likes == {"u1": {"2"}}


async def test_bad_token_raises():
    s = server()
    with pytest.raises(XError) as e:
        await HttpXClient("wrong", transport=s.transport).home_timeline("u1")
    assert e.value.status == 401


async def test_retries_429_then_succeeds():
    s = server()
    s.fail_next_with = [429]
    page = await client(s).home_timeline("u1")
    assert len(page.posts) == 5 and len(s.requests) == 2


async def test_429_exhausts_retries():
    s = server()
    s.fail_next_with = [429, 429, 429]
    with pytest.raises(RateLimited):
        await client(s, max_retries=2).home_timeline("u1")
