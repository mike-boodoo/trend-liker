"""Per-user feed agent: reads the feed, picks posts matching the user's
interests, and (only when explicitly enabled) likes them."""
from __future__ import annotations

from dataclasses import dataclass

from .finance import is_finance
from .models import Post, Profile, RunReport
from .x_client import XClient, XError


@dataclass(frozen=True)
class Policy:
    max_likes: int = 5
    max_pages: int = 3
    page_size: int = 20
    dry_run: bool = True  # liking for real must be an explicit opt-in
    # Auto-liking stock posts across accounts is coordinated amplification (pump risk).
    like_finance: bool = False


def score(post: Post, profile: Profile) -> float:
    if post.author_id in profile.blocked_authors:
        return 0.0
    words = {w.strip(".,!?#@").lower() for w in post.text.split()}
    hits = len(words & profile.interests)
    return hits + (post.like_count / 10_000 if hits else 0.0)


class FeedAgent:
    def __init__(self, client: XClient, profile: Profile, policy: Policy = Policy()):
        self.client, self.profile, self.policy = client, profile, policy

    async def fetch(self) -> list[Post]:
        posts: list[Post] = []
        token = None
        for _ in range(self.policy.max_pages):
            page = await self.client.home_timeline(
                self.profile.user_id, self.policy.page_size, token
            )
            posts += page.posts
            token = page.next_token
            if not token:
                break
        return posts

    async def run(self) -> RunReport:
        report = RunReport(self.profile.user_id, dry_run=self.policy.dry_run)
        posts = await self.fetch()
        report.seen = len(posts)
        eligible = [p for p in posts if self.policy.like_finance or not is_finance(p.text)]
        report.skipped_finance = len(posts) - len(eligible)
        ranked = sorted(
            (p for p in eligible if score(p, self.profile) > 0),
            key=lambda p: score(p, self.profile),
            reverse=True,
        )
        report.selected = ranked[: self.policy.max_likes]
        if self.policy.dry_run:
            return report
        for p in report.selected:
            try:
                if await self.client.like(self.profile.user_id, p.id):
                    report.liked.append(p.id)
            except XError as e:
                report.errors.append(f"{p.id}: {e}")
                break  # stop on API errors rather than hammering
        return report
