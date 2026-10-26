"""Local stand-in for X's OAuth 2.0 endpoints, to exercise Sign in with X without an X app.

    python -m trend_liker.mock_x_oauth --port 9999

then run the server with
    X_CLIENT_ID=mock X_AUTHORIZE_URL=http://127.0.0.1:9999/i/oauth2/authorize \
    X_TOKEN_URL=http://127.0.0.1:9999/2/oauth2/token X_API_BASE=http://127.0.0.1:9999

Authorization is auto-approved; add `login_as=<handle>` to the authorize URL to pick who
signs in (default "mock_user"). The token endpoint enforces PKCE (S256) like X does.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
import json
import secrets
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlparse


def pkce_challenge(verifier: str) -> str:
    return base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()


class MockXOAuth:
    def __init__(self, client_id: str = "mock", port: int = 0):
        self.client_id = client_id
        self.codes: dict[str, dict] = {}   # code -> challenge, redirect_uri, handle
        self.tokens: dict[str, str] = {}   # access token -> handle
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
                                            "handle": q.get("login_as", "mock_user")}
                    self.send_response(302)
                    self.send_header("location", f"{q['redirect_uri']}?{urlencode({'code': code, 'state': q['state']})}")
                    self.end_headers()
                elif u.path == "/2/users/me":
                    token = self.headers.get("authorization", "").removeprefix("Bearer ")
                    handle = mock.tokens.get(token)
                    if not handle:
                        return self._json(401, {"title": "Unauthorized"})
                    self._json(200, {"data": {"id": mock.user_id(handle), "name": handle, "username": handle}})
                else:
                    self._json(404, {"title": "Not Found"})

            def do_POST(self):
                if urlparse(self.path).path != "/2/oauth2/token":
                    return self._json(404, {"title": "Not Found"})
                body = self.rfile.read(int(self.headers.get("content-length", 0))).decode()
                f = {k: v[0] for k, v in parse_qs(body).items()}
                with mock.lock:
                    grant = mock.codes.pop(f.get("code", ""), None)  # codes are single-use
                ok = (grant and f.get("grant_type") == "authorization_code" and f.get("client_id") == mock.client_id
                      and f.get("redirect_uri") == grant["redirect_uri"]
                      and pkce_challenge(f.get("code_verifier", "")) == grant["challenge"])
                if not ok:
                    return self._json(400, {"error": "invalid_grant"})
                token = secrets.token_urlsafe(24)
                with mock.lock:
                    mock.tokens[token] = grant["handle"]
                self._json(200, {"token_type": "bearer", "access_token": token, "expires_in": 7200,
                                 "scope": "tweet.read users.read"})

        return H


if __name__ == "__main__":
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--port", type=int, default=9999)
    ap.add_argument("--client-id", default="mock")
    a = ap.parse_args()
    m = MockXOAuth(a.client_id, a.port)
    print(f"mock X OAuth on {m.url}")
    m.server.serve_forever()
