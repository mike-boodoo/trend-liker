"""Seeded synthetic feeds: what a plausible X home timeline might hold, across the
server's categories (plus some unlabeled everyday posts)."""
from __future__ import annotations

import random

from .models import Post

TEMPLATES: dict[str, list[str]] = {
    "ai": ["New results on LLM inference latency for {x}", "Fine-tuning a small transformer on {x} data",
           "Why GPT evals keep disagreeing about {x}", "Neural nets for {x}: what actually worked",
           "Distilling an LLM down for {x}", "ML ops lessons from {x}"],
    "agents": ["Agents that call MCP tools for {x}", "Agentic workflows broke our {x}",
               "Shipping an agent framework for {x} in a weekend", "Agent memory design for {x}",
               "Evaluating agents on {x} tasks"],
    "rust": ["Tokio tips for {x} services", "Why we rewrote {x} in Rust", "Cargo workspaces for {x}",
             "axum vs actix for {x}", "Rust error handling in {x}"],
    "cloud": ["Serverless cold starts on AWS for {x}", "Cutting our GCP bill for {x} by half",
              "Azure outage notes from the {x} team", "Multi-cloud {x} without the pain"],
    "kubernetes": ["Kubernetes scheduling for {x}", "Helm charts we regret in {x}", "k8s autoscaling pitfalls for {x}",
                   "Running {x} on kubectl alone"],
    "search": ["Vector search with HNSW for {x}", "Hybrid retrieval beats embeddings alone for {x}",
               "Search relevance tuning for {x}"],
    "programming": ["A compiler bug that cost our {x} team a week", "Refactor of our {x} API",
                    "Opensource library maintenance for {x}", "A framework for {x} nobody asked for"],
    "infrastructure": ["Terraform drift in {x}", "Datacenter power is the new bottleneck for {x}",
                       "Infra on-call lessons from {x}"],
    "finance": ["$NVDA earnings play this week", "My stock portfolio after {x} news", "Bullish on $TSLA options",
                "Why {x} stocks dipped today"],
    "": ["Coffee before the {x} standup", "Weekend hike photos", "Anyone else watching the game?",
         "Moving apartments again", "Best tacos near the {x} office"],
}
SUBJECTS = ["payments", "search", "billing", "observability", "a startup", "robotics", "chat", "logistics",
            "healthcare", "gaming", "ads", "fraud detection", "maps", "email", "video", "CI"]


def mock_feed(n: int = 60, seed: int = 0, weights: dict[str, float] | None = None, prefix: str = "m") -> list[Post]:
    """`n` posts. `weights` biases categories (default: uniform). Post ids are `<prefix><seed>-<i>`,
    so feeds with the same prefix and seed share posts (the cross-feed overlap signal)."""
    rng = random.Random(seed)
    labels = list(TEMPLATES)
    w = [(weights or {}).get(l, 1.0) for l in labels]
    authors = [f"author{rng.randrange(1000):03d}" for _ in range(max(8, n // 4))]
    posts, seen = [], set()
    for i in range(n):
        label = rng.choices(labels, w)[0]
        text = rng.choice(TEMPLATES[label]).format(x=rng.choice(SUBJECTS))
        for _ in range(20):  # keep posts distinct, as a real feed's are
            if text not in seen:
                break
            text = rng.choice(TEMPLATES[label]).format(x=rng.choice(SUBJECTS))
        else:
            text = f"{text} (part {i})"
        seen.add(text)
        likes = int(rng.lognormvariate(4, 1.5))
        posts.append(Post(id=f"{prefix}{seed}-{i}", author_id=rng.choice(authors), text=text, like_count=likes))
    return posts
