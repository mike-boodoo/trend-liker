"""Higher-order connector layer: one query fanned out across third-party
sources (X feed, others), with per-source timeouts and partial failures."""
from __future__ import annotations

import asyncio
from collections import Counter
from dataclasses import dataclass, field
from typing import Protocol

from .agent import FeedAgent, score


@dataclass(frozen=True)
class Query:
    user_id: str
    terms: frozenset[str]
    limit: int = 10


@dataclass(frozen=True)
class Record:
    source: str
    id: str
    text: str
    score: float


@dataclass
class FederatedResult:
    records: list[Record] = field(default_factory=list)
    errors: dict[str, str] = field(default_factory=dict)


class Connector(Protocol):
    name: str

    async def query(self, q: Query) -> list[Record]: ...


class XFeedConnector:
    """Exposes a user's X feed (read-only) as a queryable source."""

    name = "x"

    def __init__(self, agents: dict[str, FeedAgent]):
        self.agents = agents

    async def query(self, q: Query) -> list[Record]:
        agent = self.agents[q.user_id]
        profile = type(agent.profile)(q.user_id, q.terms, agent.profile.blocked_authors)
        out = [Record(self.name, p.id, p.text, score(p, profile)) for p in await agent.fetch()]
        return [r for r in out if r.score > 0]


class Registry:
    def __init__(self, timeout: float = 5.0):
        self._connectors: dict[str, Connector] = {}
        self.timeout = timeout

    def register(self, c: Connector) -> None:
        self._connectors[c.name] = c

    async def query(self, q: Query) -> FederatedResult:
        async def one(c: Connector):
            return await asyncio.wait_for(c.query(q), self.timeout)

        names = list(self._connectors)
        results = await asyncio.gather(
            *(one(self._connectors[n]) for n in names), return_exceptions=True
        )
        fr = FederatedResult()
        for name, res in zip(names, results):
            if isinstance(res, BaseException):
                fr.errors[name] = f"{type(res).__name__}: {res}"
            else:
                fr.records += res
        fr.records.sort(key=lambda r: r.score, reverse=True)
        fr.records = fr.records[: q.limit]
        return fr


def shared_topics(user_terms: dict[str, list[str]], min_users: int = 2) -> list[str]:
    """Terms present in at least `min_users` users' (opted-in) feeds."""
    counts = Counter(t for terms in user_terms.values() for t in set(terms))
    return sorted(t for t, n in counts.items() if n >= min_users)
