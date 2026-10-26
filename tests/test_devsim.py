import json
import random

import httpx

from trend_liker.devsim import (OTHER, arrow, classify, decide, load_file, movement, parse_args,
                                split_by_label, write_report, Result)
from trend_liker.mock_feed import mock_feed
from trend_liker.models import Post


def test_mock_feed_is_seeded_and_biased():
    a, b = mock_feed(40, seed=3), mock_feed(40, seed=3)
    assert [p.text for p in a] == [p.text for p in b] and len({p.id for p in a}) == 40
    assert len({p.text for p in mock_feed(200, seed=5)}) == 200, "no duplicate posts"
    assert mock_feed(5, seed=3, prefix="you")[0].id == "you3-0"
    rust = mock_feed(200, seed=1, weights={"rust": 50.0})
    assert sum("Rust" in p.text or "Tokio" in p.text or "Cargo" in p.text or "axum" in p.text for p in rust) > 120


def test_split_by_label_disjoint_by_default():
    posts = [Post("1", "a", "x"), Post("2", "a", "y"), Post("3", "a", "z")]
    labels = [["ai", "agents"], [], ["rust"]]
    assert {k: [p.id for p in v] for k, v in split_by_label(posts, labels).items()} == {"ai": ["1"], OTHER: ["2"], "rust": ["3"]}
    both = split_by_label(posts, labels, overlap=True)
    assert [p.id for p in both["agents"]] == ["1"] and [p.id for p in both["ai"]] == ["1"]


def test_decide_follows_interests_and_is_seeded():
    item = {"categories": ["rust"], "mine": []}
    runs = [[decide(random.Random(s), {"rust"}, item) for s in range(200)] for _ in range(2)]
    assert runs[0] == runs[1]
    likes = sum("like" in k for k in runs[0])
    assert 90 < likes < 150 and all(k[0] == "impression" for k in runs[0])
    off = [decide(random.Random(s), {"ai"}, item) for s in range(200)]
    assert not any("like" in k for k in off) and any("skip" in k for k in off)
    assert decide(random.Random(0), {"rust"}, {"categories": ["rust"], "mine": ["like"]}) == []


def test_load_file_accepts_post_list_and_x_response(tmp_path):
    (tmp_path / "a.json").write_text(json.dumps([{"id": 1, "author_id": "u", "text": "hi", "like_count": 3}]))
    (tmp_path / "b.json").write_text(json.dumps({"data": [{"id": "2", "author_id": "v", "text": "yo",
                                                           "public_metrics": {"like_count": 9}}]}))
    assert load_file(str(tmp_path / "a.json")) == [Post("1", "u", "hi", 3)]
    assert load_file(str(tmp_path / "b.json")) == [Post("2", "v", "yo", 9)]


async def test_classify_batches_requests():
    sizes = []

    def h(req):
        texts = json.loads(req.content)["texts"]
        sizes.append(len(texts))
        return httpx.Response(200, json={"results": [{"labels": ["rust"], "finance": False}] * len(texts)})

    out = await classify("http://s", ["t"] * 1001, httpx.MockTransport(h))
    assert sizes == [500, 500, 1] and len(out) == 1001


def test_movement_and_arrows():
    before = [{"id": "a"}, {"id": "b"}, {"id": "c"}]
    after = [{"id": "c"}, {"id": "a"}, {"id": "d"}]
    assert [d for _, d in movement(before, after)] == [2, -1, None]
    assert [arrow(d) for d in (2, -1, 0, None)] == ["+2", "-1", "=", "new"]


def test_report_escapes_post_text(tmp_path):
    item = {"id": "1", "external_id": "p1", "text": "<script>alert(1)</script>", "categories": ["ai"],
            "amplified": True, "explore": False, "score": 1.0, "likes": 0, "contributors": 1}
    r = Result("dev-1", "dev-you", [Post("p1", "a", "x")], {"ai": [Post("p1", "a", "x")]}, [], [item],
               rounds=[(3, [item])], views={"dev-you-ai": [item]})
    path = tmp_path / "r.html"
    write_report(r, str(path))
    out = path.read_text()
    assert "<script>alert" not in out and "&lt;script&gt;" in out and "your feed" in out


def test_args():
    a = parse_args(["--source", "mock", "--rounds", "2"])
    assert (a.source, a.rounds, a.you) == ("mock", 2, "dev-you")
