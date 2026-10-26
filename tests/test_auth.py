import httpx
import pytest

from trend_liker.auth import dev_login, sign_in_with_x
from trend_liker.forus import ForUsClient, ForUsError
from trend_liker.lobby import LobbyClient
from trend_liker.mock_x_oauth import MockXOAuth, pkce_challenge


def fake_server():
    """Minimal stand-in for the server's two sign-in steps, checking the state cookie."""
    def h(req: httpx.Request) -> httpx.Response:
        if req.url.path == "/auth/x/login":
            return httpx.Response(303, headers={"location": "https://x.example/authorize?state=s1",
                                                "set-cookie": "tl_oauth_state=s1; Path=/; HttpOnly"})
        if req.url.path == "/auth/x/callback":
            if "tl_oauth_state=s1" not in req.headers.get("cookie", "") or req.url.params.get("state") != "s1":
                return httpx.Response(400, text="different browser")
            return httpx.Response(303, headers={"location": "/", "set-cookie": "tl_session=tok123; Path=/; HttpOnly"})
        if req.url.path == "/auth/dev/login":
            return httpx.Response(200, json={"user_id": "alice", "token": "devtok"})
        return httpx.Response(404)
    return httpx.MockTransport(h)


async def test_sign_in_carries_state_cookie_and_returns_session():
    seen = []

    async def approve(url):
        seen.append(url)
        return "http://127.0.0.1:3000/auth/x/callback?code=c&state=s1"

    assert await sign_in_with_x(approve, base_url="http://127.0.0.1:3000", transport=fake_server()) == "tok123"
    assert seen == ["https://x.example/authorize?state=s1"]


async def test_sign_in_failure_raises():
    async def approve(url):
        return "http://127.0.0.1:3000/auth/x/callback?code=c&state=WRONG"

    with pytest.raises(ForUsError) as e:
        await sign_in_with_x(approve, base_url="http://127.0.0.1:3000", transport=fake_server())
    assert e.value.status == 400


async def test_dev_login_and_clients_send_bearer():
    assert await dev_login("alice", base_url="http://127.0.0.1:3000", transport=fake_server()) == "devtok"
    auth = []
    t = httpx.MockTransport(lambda r: (auth.append(r.headers.get("authorization")), httpx.Response(200, json={"items": [], "public_feeds": []}))[1])
    await ForUsClient(transport=t, token="tok").subscriptions("alice")
    await LobbyClient(transport=t, token="tok").feed("ai", "alice")
    await ForUsClient(transport=t).subscriptions("alice")
    assert auth == ["Bearer tok", "Bearer tok", None]


def test_mock_x_oauth_enforces_pkce_and_single_use_codes():
    m = MockXOAuth("cid").start()
    try:
        with httpx.Client(base_url=m.url, follow_redirects=False) as c:
            verifier = "v" * 43
            r = c.get("/i/oauth2/authorize", params={
                "client_id": "cid", "code_challenge_method": "S256", "code_challenge": pkce_challenge(verifier),
                "redirect_uri": "http://app/cb", "state": "st", "login_as": "alice"})
            code = httpx.URL(r.headers["location"]).params["code"]
            form = {"grant_type": "authorization_code", "client_id": "cid", "redirect_uri": "http://app/cb", "code": code}
            assert c.post("/2/oauth2/token", data={**form, "code_verifier": "wrong" * 9}).status_code == 400
            # The failed attempt consumed the code, like a real authorization server.
            assert c.post("/2/oauth2/token", data={**form, "code_verifier": verifier}).status_code == 400
            r = c.get("/i/oauth2/authorize", params={
                "client_id": "cid", "code_challenge_method": "S256", "code_challenge": pkce_challenge(verifier),
                "redirect_uri": "http://app/cb", "state": "st", "login_as": "alice"})
            form["code"] = httpx.URL(r.headers["location"]).params["code"]
            token = c.post("/2/oauth2/token", data={**form, "code_verifier": verifier}).json()["access_token"]
            me = c.get("/2/users/me", headers={"authorization": f"Bearer {token}"}).json()["data"]
            assert me["username"] == "alice" and me["id"] == m.user_id("alice")
            assert c.get("/2/users/me").status_code == 401
    finally:
        m.stop()
