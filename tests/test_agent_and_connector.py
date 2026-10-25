import asyncio

from trend_liker.agent import FeedAgent, Policy
from trend_liker.connector import Query, Registry, XFeedConnector, shared_topics
from trend_liker.mock_x import MockXServer, make_tweet
from trend_liker.models import Profile
from trend_liker.x_client import HttpXClient


def setup(policy=Policy(), interests=("python", "ai")):
    tl = [
        make_tweet(1, "Python tips", likes=50),
        make_tweet(2, "lunch pic"),
        make_tweet(3, "AI and Python together", likes=10),
        make_tweet(4, "ai spam", author="bad"),
        make_tweet(5, "more AI news"),
    ]
    s = MockXServer({"u1": tl})
    prof = Profile("u1", frozenset(interests), frozenset({"bad"}))
    agent = FeedAgent(HttpXClient("test-token", transport=s.transport), prof, policy)
    return s, agent


async def test_dry_run_never_likes():
    s, agent = setup()
    r = await agent.run()
    assert r.dry_run and r.liked == [] and s.likes == {}
    assert [p.id for p in r.selected] == ["3", "1", "5"]  # blocked + irrelevant dropped
    assert all(q.method == "GET" for q in s.requests)


async def test_live_run_likes_capped():
    s, agent = setup(Policy(max_likes=2, dry_run=False))
    r = await agent.run()
    assert r.liked == ["3", "1"] and s.likes["u1"] == {"3", "1"}


async def test_live_run_stops_on_like_error():
    s, agent = setup(Policy(dry_run=False))
    s.fail_likes_with = 403
    r = await agent.run()
    assert r.liked == [] and len(r.errors) == 1  # stopped after first failure
    assert sum(q.method == "POST" for q in s.requests) == 1


async def test_paging_limit():
    s, agent = setup(Policy(page_size=2, max_pages=2))
    r = await agent.run()
    assert r.seen == 4 and len(s.requests) == 2


class Slow:
    name = "slow"

    async def query(self, q):
        await asyncio.sleep(1)
        return []


class Fixed:
    name = "fixed"

    async def query(self, q):
        from trend_liker.connector import Record
        return [Record("fixed", "f1", "python db row", 99.0)]


async def test_federated_query_merges_and_isolates_failures():
    _, agent = setup()
    reg = Registry(timeout=0.05)
    reg.register(XFeedConnector({"u1": agent}))
    reg.register(Fixed())
    reg.register(Slow())
    res = await reg.query(Query("u1", frozenset({"python"}), limit=3))
    assert [r.id for r in res.records][0] == "f1"  # highest score first
    assert {r.source for r in res.records} == {"fixed", "x"}
    assert "slow" in res.errors and "TimeoutError" in res.errors["slow"]


def test_shared_topics():
    out = shared_topics({"a": ["ai", "go"], "b": ["ai", "rust"], "c": ["go", "ai"]}, 3)
    assert out == ["ai"]
