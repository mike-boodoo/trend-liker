"""Local stand-in for X's OAuth 2.0 endpoints, to exercise Sign in with X without an X app.

    python -m trend_liker.mock_x_oauth --port 9999

then run the server with
    X_CLIENT_ID=mock X_AUTHORIZE_URL=http://127.0.0.1:9999/i/oauth2/authorize \
    X_TOKEN_URL=http://127.0.0.1:9999/2/oauth2/token X_API_BASE=http://127.0.0.1:9999

Authorization is auto-approved; add `login_as=<handle>` to the authorize URL to pick who
signs in (default "mock_user"). The token endpoint enforces PKCE (S256) like X does, issues
rotating refresh tokens when `offline.access` is requested, and access tokens expire after
`--token-ttl` seconds. Each handle gets a seeded home timeline at
/2/users/{id}/timelines/reverse_chronological (from trend_liker.mock_feed).
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import secrets
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import time
from urllib.parse import parse_qs, urlencode, urlparse

from .mock_feed import mock_feed


def pkce_challenge(verifier: str) -> str:
    return base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()


class MockXOAuth:
    def __init__(self, client_id: str = "mock", port: int = 0, token_ttl: int = 7200, timeline_posts: int = 120):
        self.client_id, self.token_ttl, self.timeline_posts = client_id, token_ttl, timeline_posts
        self.codes: dict[str, dict] = {}   # code -> challenge, redirect_uri, handle, scope
        self.tokens: dict[str, tuple[str, float]] = {}  # access token -> (handle, expires at)
        self.refresh: dict[str, tuple[str, str]] = {}   # refresh token -> (handle, scope)
        self.revoked = 0
        self.ids: dict[str, str] = {}      # handle -> stable numeric id
        self.lock = threading.Lock()
        self.server = ThreadingHTTPServer(("127.0.0.1", port), self._handler())
        self.port = self.server.server_address[1]
        self.url = f"http://127.0.0.1:{self.port}"

    def start(self) -> "MockXOAuth":
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self

    def stop(self) -> None:
        self.server.shutdown()

    def user_id(self, handle: str) -> str:
        with self.lock:
            return self.ids.setdefault(handle, str(1000 + len(self.ids)))

    def _handler(self):
        mock = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):  # quiet
                pass

            def _json(self, status: int, body: dict):
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_GET(self):
                u = urlparse(self.path)
                if u.path == "/i/oauth2/authorize":
                    q = {k: v[0] for k, v in parse_qs(u.query).items()}
                    if q.get("client_id") != mock.client_id or q.get("code_challenge_method") != "S256":
                        return self._json(400, {"error": "invalid_request"})
                    code = secrets.token_urlsafe(16)
                    with mock.lock:
                        mock.codes[code] = {"challenge": q["code_challenge"], "redirect_uri": q["redirect_uri"],
                                            "handle": q.get("login_as", "mock_user"), "scope": q.get("scope", "")}
                    self.send_response(302)
                    self.send_header("location", f"{q['redirect_uri']}?{urlencode({'code': code, 'state': q['state']})}")
                    self.end_headers()
                elif u.path == "/2/users/me":
                    handle = self._caller()
                    if not handle:
                        return self._json(401, {"title": "Unauthorized"})
                    self._json(200, {"data": {"id": mock.user_id(handle), "name": handle, "username": handle}})
                elif u.path.startswith("/2/users/") and u.path.endswith("/timelines/reverse_chronological"):
                    handle = self._caller()
                    uid = u.path.split("/")[3]
                    if not handle or mock.user_id(handle) != uid:
                        return self._json(401, {"title": "Unauthorized"})
                    q = {k: v[0] for k, v in parse_qs(u.query).items()}
                    start, size = int(q.get("pagination_token", 0)), min(int(q.get("max_results", 100)), 100)
                    feed = mock_feed(mock.timeline_posts, seed=sum(map(ord, handle)), prefix=f"x{handle}-")
                    page = feed[start : start + size]
                    data = [{"id": p.id, "text": p.text, "author_id": p.author_id, "created_at": "2026-10-26T12:00:00Z",
                             "public_metrics": {"like_count": p.like_count, "retweet_count": p.like_count // 7,
                                                "reply_count": p.like_count // 20}} for p in page]
                    users = [{"id": a, "username": a, "protected": a.endswith("7")} for a in sorted({p.author_id for p in page})]
                    meta = {"result_count": len(data)}
                    if start + size < len(feed):
                        meta["next_token"] = str(start + size)
                    self._json(200, {"data": data, "meta": meta, "includes": {"users": users}})
                else:
                    self._json(404, {"title": "Not Found"})

            def _caller(self) -> str | None:
                token = self.headers.get("authorization", "").removeprefix("Bearer ")
                handle, expires = mock.tokens.get(token, (None, 0.0))
                return handle if expires > time.time() else None

            def do_POST(self):
                path = urlparse(self.path).path
                body = self.rfile.read(int(self.headers.get("content-length", 0))).decode()
                f = {k: v[0] for k, v in parse_qs(body).items()}
                if path == "/2/oauth2/revoke":
                    with mock.lock:
                        mock.refresh.pop(f.get("token", ""), None)
                        mock.revoked += 1
                    return self._json(200, {"revoked": True})
                if path != "/2/oauth2/token":
                    return self._json(404, {"title": "Not Found"})
                if f.get("client_id") != mock.client_id:
                    return self._json(401, {"error": "invalid_client"})
                with mock.lock:
                    if f.get("grant_type") == "refresh_token":
                        found = mock.refresh.pop(f.get("refresh_token", ""), None)  # rotation: single use
                        grant = {"handle": found[0], "scope": found[1]} if found else None
                    else:
                        grant = mock.codes.pop(f.get("code", ""), None)  # codes are single-use
                        if grant and not (f.get("grant_type") == "authorization_code"
                                          and f.get("redirect_uri") == grant["redirect_uri"]
                                          and pkce_challenge(f.get("code_verifier", "")) == grant["challenge"]):
                            grant = None
                if not grant:
                    return self._json(400, {"error": "invalid_grant"})
                token = secrets.token_urlsafe(24)
                out = {"token_type": "bearer", "access_token": token, "expires_in": mock.token_ttl, "scope": grant["scope"]}
                with mock.lock:
                    mock.tokens[token] = (grant["handle"], time.time() + mock.token_ttl)
                    if "offline.access" in grant["scope"].split():
                        out["refresh_token"] = secrets.token_urlsafe(24)
                        mock.refresh[out["refresh_token"]] = (grant["handle"], grant["scope"])
                self._json(200, out)

        return H


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", type=int, default=9999)
    ap.add_argument("--client-id", default="mock")
    ap.add_argument("--token-ttl", type=int, default=7200, help="access token lifetime in seconds")
    a = ap.parse_args()
    m = MockXOAuth(a.client_id, a.port, token_ttl=a.token_ttl)
    print(f"mock X OAuth on {m.url}")
    m.server.serve_forever()
