from __future__ import annotations

from dataclasses import dataclass, field


@dataclass(frozen=True)
class Post:
    id: str
    author_id: str
    text: str
    like_count: int = 0
    created_at: str = ""


@dataclass(frozen=True)
class Page:
    posts: list[Post]
    next_token: str | None = None


@dataclass(frozen=True)
class Profile:
    """A user's stated tastes. The agent only acts on what the user opted into."""

    user_id: str
    interests: frozenset[str] = frozenset()
    blocked_authors: frozenset[str] = frozenset()


@dataclass
class RunReport:
    user_id: str
    seen: int = 0
    selected: list[Post] = field(default_factory=list)
    liked: list[str] = field(default_factory=list)
    dry_run: bool = True
    errors: list[str] = field(default_factory=list)
