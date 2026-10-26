"""Finance/stock content detection. Mirrors universal-feed/src/classify.rs."""
from __future__ import annotations

import re

KEYWORDS = frozenset({
    "stock", "stocks", "earnings", "shares", "dividend", "dividends", "ipo", "bullish",
    "bearish", "nasdaq", "portfolio", "trading", "ticker", "etf", "options",
})
_CASHTAG = re.compile(r"^\$[A-Za-z]{1,5}$")


def is_finance(text: str) -> bool:
    words = {w.lower() for w in re.split(r"[^0-9A-Za-z]+", text) if w}
    if words & KEYWORDS:
        return True
    return any(_CASHTAG.match(w.strip(".,!?;:'\"()[]")) for w in text.split())
