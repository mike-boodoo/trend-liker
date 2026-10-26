"""Dev testing flow: see your own feed through the service.

Your feed (a seeded mock, a JSON file, or your real X timeline) is split into the
server's categories. Each slice becomes a test account that joins a lobby as if it
were someone else's feed, optionally alongside other test accounts with their own
mock feeds. Simulated voting rounds then re-rank the pool, and you see how it
moves for you and from each slice's point of view. Afterwards, open the printed
URLs to sign in as any test account and vote yourself while the ranking updates live.

    ./scripts/dev.sh                       # start the server in dev mode and run this
    python -m trend_liker.devsim --help    # against a server you started yourself

The server must run with AUTH_DEV_LOGIN=1 (test accounts sign in without X).
"""
from __future__ import annotations

import argparse
import asyncio
import html
import json
import os
import random
import time
from dataclasses import dataclass, field
from pathlib import Path

import httpx

from .agent import FeedAgent, Policy
from .auth import dev_login
from .forus import DEFAULT_URL, ForUsClient
from .lobby import LobbyClient
from .mock_feed import mock_feed
from .models import Post, Profile
from .x_client import HttpXClient

OTHER = "other"  # slice for posts the classifier gives no label


# ---- loading and splitting ------------------------------------------------------------

def load_file(path: str) -> list[Post]:
    """A JSON list of posts, or an X API response ({"data": [...]})."""
    raw = json.loads(Path(path).read_text())
    rows = raw.get("data", []) if isinstance(raw, dict) else raw
    return [
        Post(id=str(r["id"]), author_id=str(r.get("author_id", r.get("author", ""))), text=r["text"],
             like_count=int(r.get("like_count", r.get("public_metrics", {}).get("like_count", 0))),
             protected=bool(r.get("protected", False)))
        for r in rows
    ]


async def load_x(token: str, max_pages: int) -> list[Post]:
    """Your real home timeline. Needs a user-context OAuth 2.0 token with tweet.read users.read."""
    x = HttpXClient(token)
    try:
        me = await x.me()
        return await FeedAgent(x, Profile(me["id"]), Policy(max_pages=max_pages, page_size=100)).fetch()
    finally:
        await x.aclose()


async def classify(base_url: str, texts: list[str], transport: httpx.AsyncBaseTransport | None = None) -> list[list[str]]:
    """The server's labels for each text, so slices match how the server will see them."""
    out: list[list[str]] = []
    async with httpx.AsyncClient(base_url=base_url, transport=transport, timeout=10.0) as c:
        for i in range(0, len(texts), 500):
            r = await c.post("/api/classify", json={"texts": texts[i : i + 500]})
            r.raise_for_status()
            out += [x["labels"] for x in r.json()["results"]]
    return out


def split_by_label(posts: list[Post], labels: list[list[str]], overlap: bool = False) -> dict[str, list[Post]]:
    """Slice a feed by label. By default each post goes only to its first label, so slices
    don't share posts (which the lobby would count as cross-feed overlap)."""
    slices: dict[str, list[Post]] = {}
    for post, ls in zip(posts, labels, strict=True):
        for label in (ls if overlap else ls[:1]) or [OTHER]:
            slices.setdefault(label, []).append(post)
    return dict(sorted(slices.items()))


# ---- simulated members ------------------------------------------------------------------

def decide(rng: random.Random, interests: set[str], item: dict) -> list[str]:
    """One simulated member reacting to a post in their lobby feed: likes (and sometimes
    bookmarks) what matches their interests, occasionally skips what doesn't, and has seen it."""
    if item.get("mine"):
        return []  # already reacted
    kinds = ["impression"]
    if interests & set(item["categories"]):
        if rng.random() < 0.6:
            kinds.append("like")
            if rng.random() < 0.25:
                kinds.append("bookmark")
    elif rng.random() < 0.2:
        kinds.append("skip")
    return kinds


@dataclass
class Member:
    user: str
    interests: set[str]
    feed: list[Post]
    client: LobbyClient
    kind: str  # "slice" (a slice of your feed) or "other" (another test account)


async def run_round(lobby: str, members: list[Member], rng: random.Random, top: int) -> int:
    votes = 0
    for m in members:
        for item in (await m.client.feed(lobby, m.user, limit=top))["items"]:
            for kind in decide(rng, m.interests, item):
                await m.client.interact(lobby, m.user, item["id"], kind)
                votes += kind != "impression"
    return votes


# ---- the flow ---------------------------------------------------------------------------

@dataclass
class Result:
    lobby: str
    you: str
    feed: list[Post]
    slices: dict[str, list[Post]]
    members: list[Member]
    before: list[dict]
    rounds: list[tuple[int, list[dict]]] = field(default_factory=list)  # (votes, your view after)
    views: dict[str, list[dict]] = field(default_factory=dict)          # member -> their view at the end


async def run(args: argparse.Namespace, transport: httpx.AsyncBaseTransport | None = None) -> Result:
    base, you = args.server, args.you
    rng = random.Random(args.seed)
    if args.source == "file":
        feed = load_file(args.file)
    elif args.source == "x":
        token = os.environ.get("X_USER_TOKEN")
        if not token:
            raise SystemExit("--source x needs X_USER_TOKEN (a user-context OAuth 2.0 token: tweet.read users.read)")
        feed = await load_x(token, args.x_max_pages)
    else:
        feed = mock_feed(args.posts, seed=args.seed, prefix="you")

    slices = split_by_label(feed, await classify(base, [p.text for p in feed], transport), args.overlap)
    lobby = args.lobby or f"dev-{int(time.time())}"
    policy = json.loads(Path(args.policy).read_text()) if args.policy else None

    async def sign_in(user: str, publish: bool) -> LobbyClient:
        token = await dev_login(user, base, transport)
        if publish:
            await ForUsClient(base, transport, token).set_publishing(user, True)
        return LobbyClient(base, transport, token)

    you_client = await sign_in(you, args.you_contribute)
    await you_client.create(lobby, f"Dev lobby for {you}", you, policy)
    members = [Member(f"{you}-{label}", {label}, posts, await sign_in(f"{you}-{label}", True), "slice")
               for label, posts in slices.items()]
    topical = [l for l in slices if l != OTHER] or [OTHER]
    for i in range(args.others):
        interests = set(rng.sample(topical, k=min(2, len(topical))))
        own = mock_feed(args.posts // 2, seed=args.seed + 100 + i, weights={l: 4.0 for l in interests}, prefix="o")
        shared = rng.sample(feed, k=len(feed) // 5)  # their X surfaced some of the same posts you saw
        user = f"dev-other{i + 1}"
        members.append(Member(user, interests, own + shared, await sign_in(user, True), "other"))
    for m in members:
        await m.client.join(lobby, m.user)
        await m.client.contribute(lobby, m.user, m.feed)
    if args.you_contribute:
        await you_client.contribute(lobby, you, feed)

    result = Result(lobby, you, feed, slices, members, (await you_client.feed(lobby, you, limit=args.top))["items"])
    for _ in range(args.rounds):
        votes = await run_round(lobby, members, rng, args.top)
        result.rounds.append((votes, (await you_client.feed(lobby, you, limit=args.top))["items"]))
    for m in members:
        result.views[m.user] = (await m.client.feed(lobby, m.user, limit=5))["items"]
    return result


# ---- output -----------------------------------------------------------------------------

def movement(before: list[dict], after: list[dict]) -> list[tuple[dict, int | None]]:
    """Each post in `after` with how many places it rose (None = new to the top)."""
    pos = {i["id"]: n for n, i in enumerate(before)}
    return [(i, pos[i["id"]] - n if i["id"] in pos else None) for n, i in enumerate(after)]


def arrow(d: int | None) -> str:
    return "new" if d is None else ("=" if d == 0 else (f"+{d}" if d > 0 else str(d)))


def print_summary(r: Result, base: str) -> None:
    yours = {p.id for p in r.feed}
    print(f"\nYour feed: {len(r.feed)} posts, split into {len(r.slices)} slices:")
    for label, posts in r.slices.items():
        print(f"  {r.you}-{label:<16} {len(posts):>3} posts")
    others = [m for m in r.members if m.kind == "other"]
    for m in others:
        print(f"  {m.user:<20} {len(m.feed):>3} posts, interested in {', '.join(sorted(m.interests))}")
    after = r.rounds[-1][1] if r.rounds else r.before
    print(f"\nYour view of lobby '{r.lobby}' after {len(r.rounds)} rounds "
          f"({sum(v for v, _ in r.rounds)} votes); movement vs before any votes:")
    for n, (i, d) in enumerate(movement(r.before, after), 1):
        src = "you" if i["external_id"] in yours else "   "
        flag = (" [finance: not amplified]" if not i["amplified"] else "") + (" [explore]" if i["explore"] else "")
        print(f"  {n:>2}. {arrow(d):>4}  {i['score']:+6.2f}  {src}  {','.join(i['categories']) or '-':<22} {i['text'][:48]}{flag}")
    print("\nHow each slice sees the lobby now (top 3):")
    for user, items in r.views.items():
        print(f"  {user}: " + " | ".join(i["text"][:28] for i in items[:3]))
    users = ",".join([r.you] + [m.user for m in r.members])
    print(f"\nWatch and vote live:\n  1. {base}/dev?users={users}   (pick a test account)\n  2. {base}/lobbies/{r.lobby}\n")


def write_report(r: Result, path: str) -> None:
    e = html.escape
    yours = {p.id for p in r.feed}

    def table(items: list[dict], moves: list[tuple[dict, int | None]] | None = None) -> str:
        rows = []
        for n, i in enumerate(items, 1):
            d = arrow(moves[n - 1][1]) if moves else ""
            tags = "".join(f'<span class="tag">{e(c)}</span>' for c in i["categories"])
            note = "" if i["amplified"] else '<span class="tag warn">finance: not amplified</span>'
            note += '<span class="tag">explore</span>' if i.get("explore") else ""
            mine = '<span class="tag you">your feed</span>' if i["external_id"] in yours else ""
            rows.append(f"<tr><td>{n}</td><td class='num'>{e(d)}</td><td class='num'>{i['score']:.2f}</td>"
                        f"<td>{e(i['text'])}<div>{mine}{tags}{note}</div></td><td class='num'>{i['likes']}</td>"
                        f"<td class='num'>{i['contributors']}</td></tr>")
        return ("<table><tr><th>#</th><th>move</th><th>score</th><th>post</th><th>likes</th><th>feeds</th></tr>"
                + "".join(rows) + "</table>")

    after = r.rounds[-1][1] if r.rounds else r.before
    slices = "".join(f"<li><b>{e(r.you)}-{e(l)}</b>: {len(p)} posts</li>" for l, p in r.slices.items())
    others = "".join(f"<li><b>{e(m.user)}</b>: {len(m.feed)} posts, likes {e(', '.join(sorted(m.interests)))}</li>"
                     for m in r.members if m.kind == "other")
    views = "".join(f"<h3>{e(u)}</h3>{table(items)}" for u, items in r.views.items())
    rounds = "".join(f"<li>Round {n}: {v} votes</li>" for n, (v, _) in enumerate(r.rounds, 1))
    Path(path).write_text(f"""<!DOCTYPE html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>Feed Simulation</title><style>
:root{{--bg:#f6f6f4;--card:#fff;--ink:#16181d;--muted:#6b7080;--line:#e3e3df;--accent:#2a5bd7;--warn:#a15c00}}
@media (prefers-color-scheme:dark){{:root{{--bg:#121316;--card:#1b1d22;--ink:#e9eaee;--muted:#9097a6;--line:#2b2e35;--accent:#7aa2ff;--warn:#f0b35a}}}}
body{{font:15px/1.45 system-ui,sans-serif;max-width:960px;margin:auto;padding:24px 16px;background:var(--bg);color:var(--ink)}}
table{{width:100%;border-collapse:collapse;background:var(--card);margin:8px 0 20px}}td,th{{border-bottom:1px solid var(--line);padding:6px 8px;text-align:left;vertical-align:top}}
.num{{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}}.tag{{display:inline-block;border:1px solid var(--line);border-radius:6px;padding:0 6px;margin:3px 4px 0 0;font-size:12px}}
.warn{{color:var(--warn)}}.you{{color:var(--accent)}}.muted{{color:var(--muted)}}
</style></head><body>
<h1>Feed simulation: {e(r.lobby)}</h1>
<p class="muted">Your feed was split into slices, each joining the lobby as a test account; {len(r.rounds)} rounds of simulated votes re-ranked the pool.</p>
<h2>Members</h2><ul>{slices}{others}</ul><ul class="muted">{rounds}</ul>
<h2>Your view after voting</h2><p class="muted">"move" is places gained since before any votes.</p>{table(after, movement(r.before, after))}
<h2>Your view before voting</h2>{table(r.before)}
<h2>How each member sees it now</h2>{views}
</body></html>""")


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    ap = argparse.ArgumentParser(prog="python -m trend_liker.devsim", description=__doc__.split("\n\n")[0])
    ap.add_argument("--server", default=DEFAULT_URL)
    ap.add_argument("--you", default="dev-you", help="your test account id")
    ap.add_argument("--source", choices=["mock", "file", "x"], default="mock",
                    help="mock: generated feed; file: --file JSON; x: your real timeline via X_USER_TOKEN")
    ap.add_argument("--file", help="JSON list of posts or an X API timeline response")
    ap.add_argument("--posts", type=int, default=60, help="mock feed size")
    ap.add_argument("--seed", type=int, default=7)
    ap.add_argument("--others", type=int, default=2, help="extra test accounts with their own mock feeds")
    ap.add_argument("--rounds", type=int, default=3)
    ap.add_argument("--top", type=int, default=10, help="how far down their feed each member reads per round")
    ap.add_argument("--lobby", help="lobby id (default: dev-<timestamp>)")
    ap.add_argument("--policy", help="JSON file with a lobby policy")
    ap.add_argument("--overlap", action="store_true", help="a post with several labels joins every matching slice")
    ap.add_argument("--you-contribute", action="store_true", help="also contribute your whole feed as yourself")
    ap.add_argument("--x-max-pages", type=int, default=2)
    ap.add_argument("--report", default="devsim-report.html", help="HTML report path ('' to skip)")
    a = ap.parse_args(argv)
    if a.source == "file" and not a.file:
        ap.error("--source file needs --file")
    return a


def main(argv: list[str] | None = None) -> None:
    args = parse_args(argv)
    result = asyncio.run(run(args))
    print_summary(result, args.server)
    if args.report:
        write_report(result, args.report)
        print(f"Report: {Path(args.report).resolve()}")


if __name__ == "__main__":
    main()
