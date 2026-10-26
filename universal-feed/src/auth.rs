//! Sign in with X (OAuth 2.0 authorization code + PKCE) and session handling.
//!
//! Sessions are random 256-bit tokens, sent as an HttpOnly cookie or as
//! `Authorization: Bearer`. Only their SHA-256 is stored. X's own access token
//! is used once, to look up who signed in, and is not kept.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::{
    extract::{FromRequestParts, Query, State},
    http::{header, request::Parts, HeaderMap, StatusCode},
    response::{AppendHeaders, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{internal, AppState};

pub const SESSION_COOKIE: &str = "tl_session";
const STATE_COOKIE: &str = "tl_oauth_state";
const SESSION_DAYS: i64 = 30;
const PENDING_TTL_SECS: i64 = 600;
const MAX_PENDING: usize = 10_000;

#[derive(Clone, Debug)]
pub struct XAuthConfig {
    pub client_id: String,
    /// Set for a confidential client (sent as HTTP Basic auth to the token endpoint).
    pub client_secret: Option<String>,
    pub redirect_uri: String,
    pub authorize_url: String,
    pub token_url: String,
    pub api_base: String,
    pub scopes: String,
}

impl XAuthConfig {
    /// None unless X_CLIENT_ID is set. Endpoint URLs can be overridden (e.g. to a mock).
    pub fn from_env() -> Option<Self> {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Some(Self {
            client_id: var("X_CLIENT_ID")?,
            client_secret: var("X_CLIENT_SECRET"),
            redirect_uri: var("X_REDIRECT_URI").unwrap_or_else(|| "http://127.0.0.1:3000/auth/x/callback".into()),
            authorize_url: var("X_AUTHORIZE_URL").unwrap_or_else(|| "https://x.com/i/oauth2/authorize".into()),
            token_url: var("X_TOKEN_URL").unwrap_or_else(|| "https://api.x.com/2/oauth2/token".into()),
            api_base: var("X_API_BASE").unwrap_or_else(|| "https://api.x.com".into()),
            scopes: var("X_SCOPES").unwrap_or_else(|| "tweet.read users.read".into()),
        })
    }
}

struct Pending { verifier: String, created: DateTime<Utc> }

pub struct Auth {
    pub x: Option<XAuthConfig>,
    /// `POST /auth/dev/login` lets anyone sign in as anyone. Local development only.
    pub dev_login: bool,
    http: reqwest::Client,
    pending: Mutex<HashMap<String, Pending>>,
}

impl Auth {
    pub fn new(x: Option<XAuthConfig>, dev_login: bool, http: reqwest::Client) -> Self {
        Self { x, dev_login, http, pending: Mutex::new(HashMap::new()) }
    }

    pub fn from_env() -> Self {
        let dev = std::env::var("AUTH_DEV_LOGIN").is_ok_and(|v| v == "1");
        Self::new(XAuthConfig::from_env(), dev, reqwest::Client::new())
    }

    /// Cookies are `Secure` unless the callback itself is plain http (local development).
    fn secure_cookies(&self) -> bool {
        self.x.as_ref().is_none_or(|x| !x.redirect_uri.starts_with("http://"))
    }

    fn cookie(&self, name: &str, value: &str, max_age_secs: i64) -> String {
        let secure = if self.secure_cookies() { "; Secure" } else { "" };
        format!("{name}={value}; HttpOnly; SameSite=Lax; Path=/; Max-Age={max_age_secs}{secure}")
    }
}

fn random_token() -> String {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).expect("OS randomness unavailable");
    URL_SAFE_NO_PAD.encode(buf)
}

pub fn hash_token(token: &str) -> Vec<u8> { Sha256::digest(token.as_bytes()).to_vec() }

/// RFC 7636 S256 code challenge.
pub fn pkce_challenge(verifier: &str) -> String { URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())) }

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get_all(header::COOKIE).iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v)
}

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "))
        .or_else(|| cookie_value(headers, SESSION_COOKIE))
}

/// The signed-in user. Rejects with 401 when there's no valid session.
pub struct AuthUser(pub String);

impl AuthUser {
    /// Requests may still name a user (older clients do); it must be the signed-in one.
    pub fn claim(&self, named: Option<&str>) -> Result<&str, StatusCode> {
        match named {
            Some(u) if u != self.0 => Err(StatusCode::FORBIDDEN),
            _ => Ok(&self.0),
        }
    }
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = StatusCode;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let token = session_token(&parts.headers).ok_or(StatusCode::UNAUTHORIZED)?;
        state.repo.session_user(&hash_token(token), Utc::now()).await.map_err(internal)?
            .map(AuthUser).ok_or(StatusCode::UNAUTHORIZED)
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/x/login", get(login))
        .route("/auth/x/callback", get(callback))
        .route("/auth/dev/login", post(dev_login))
        .route("/auth/logout", post(logout))
        .route("/api/me", get(me))
}

/// Start sign-in: remember a PKCE verifier under a fresh `state`, bind that state to this
/// browser with a short-lived cookie, and send the browser to X.
async fn login(State(state): State<AppState>) -> Result<Response, StatusCode> {
    let x = state.auth.x.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let (oauth_state, verifier) = (random_token(), random_token());
    {
        let mut pending = state.auth.pending.lock().unwrap();
        let cutoff = Utc::now() - Duration::seconds(PENDING_TTL_SECS);
        pending.retain(|_, p| p.created > cutoff);
        if pending.len() >= MAX_PENDING { return Err(StatusCode::SERVICE_UNAVAILABLE); }
        pending.insert(oauth_state.clone(), Pending { verifier: verifier.clone(), created: Utc::now() });
    }
    let url = reqwest::Url::parse_with_params(&x.authorize_url, &[
        ("response_type", "code"),
        ("client_id", x.client_id.as_str()),
        ("redirect_uri", x.redirect_uri.as_str()),
        ("scope", x.scopes.as_str()),
        ("state", oauth_state.as_str()),
        ("code_challenge", pkce_challenge(&verifier).as_str()),
        ("code_challenge_method", "S256"),
    ]).map_err(internal)?;
    let cookie = state.auth.cookie(STATE_COOKIE, &oauth_state, PENDING_TTL_SECS);
    Ok(([(header::SET_COOKIE, cookie)], Redirect::to(url.as_str())).into_response())
}

#[derive(Deserialize)]
struct Callback { code: Option<String>, state: Option<String>, error: Option<String> }

#[derive(Deserialize)]
struct TokenResponse { access_token: String }

#[derive(Deserialize)]
struct Me { data: MeData }
#[derive(Deserialize)]
struct MeData { id: String, username: String }

/// X sends the browser back here. Check `state` (one-time, unexpired, and matching this
/// browser's cookie), trade the code for a token, ask X who it is, and start a session.
async fn callback(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<Callback>) -> Result<Response, Response> {
    let bad = |msg: &str| (StatusCode::BAD_REQUEST, msg.to_string()).into_response();
    let x = state.auth.x.as_ref().ok_or(StatusCode::NOT_FOUND.into_response())?;
    if let Some(err) = q.error { return Err(bad(&format!("sign-in was not completed: {err}"))); }
    let (Some(code), Some(oauth_state)) = (q.code, q.state) else { return Err(bad("missing code or state")) };
    let pending = state.auth.pending.lock().unwrap().remove(&oauth_state);
    let Some(pending) = pending.filter(|p| p.created > Utc::now() - Duration::seconds(PENDING_TTL_SECS)) else {
        return Err(bad("sign-in expired or already used; start again"));
    };
    if cookie_value(&headers, STATE_COOKIE) != Some(oauth_state.as_str()) {
        return Err(bad("sign-in was started in a different browser"));
    }

    let mut req = state.auth.http.post(&x.token_url).form(&[
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", x.redirect_uri.as_str()),
        ("client_id", x.client_id.as_str()),
        ("code_verifier", pending.verifier.as_str()),
    ]);
    if let Some(secret) = &x.client_secret { req = req.basic_auth(&x.client_id, Some(secret)); }
    let upstream = |e: reqwest::Error| { eprintln!("X sign-in failed: {e}"); StatusCode::BAD_GATEWAY.into_response() };
    let token: TokenResponse = req.send().await.and_then(|r| r.error_for_status()).map_err(upstream)?
        .json().await.map_err(upstream)?;
    let me: Me = state.auth.http.get(format!("{}/2/users/me", x.api_base)).bearer_auth(&token.access_token)
        .send().await.and_then(|r| r.error_for_status()).map_err(upstream)?
        .json().await.map_err(upstream)?;

    let user = state.repo.upsert_x_user(&me.data.id, &me.data.username).await
        .map_err(|e| internal(e).into_response())?;
    let session = start_session(&state, &user).await.map_err(IntoResponse::into_response)?;
    let clear_state = state.auth.cookie(STATE_COOKIE, "", 0);
    // AppendHeaders: a plain header array would let the second Set-Cookie replace the first.
    Ok((AppendHeaders([(header::SET_COOKIE, session), (header::SET_COOKIE, clear_state)]), Redirect::to("/")).into_response())
}

/// Returns the Set-Cookie value; the token is also usable as a Bearer token.
async fn start_session(state: &AppState, user: &str) -> Result<String, StatusCode> {
    let token = random_token();
    let expires = Utc::now() + Duration::days(SESSION_DAYS);
    state.repo.create_session(&hash_token(&token), user, expires).await.map_err(internal)?;
    Ok(state.auth.cookie(SESSION_COOKIE, &token, SESSION_DAYS * 86_400))
}

#[derive(Deserialize)]
struct DevLogin { user_id: String }

/// Local development only (AUTH_DEV_LOGIN=1): sign in as any user id, creating it if needed.
async fn dev_login(State(state): State<AppState>, Json(r): Json<DevLogin>) -> Result<Response, StatusCode> {
    if !state.auth.dev_login { return Err(StatusCode::NOT_FOUND); }
    if r.user_id.is_empty() || r.user_id.len() > 64 || r.user_id.starts_with("x-") {
        return Err(StatusCode::BAD_REQUEST); // `x-` ids belong to real X accounts
    }
    state.repo.ensure_user(&r.user_id).await.map_err(internal)?;
    let cookie = start_session(&state, &r.user_id).await?;
    let token = cookie.split_once('=').and_then(|(_, rest)| rest.split(';').next()).unwrap_or_default().to_string();
    Ok(([(header::SET_COOKIE, cookie)], Json(json!({ "user_id": r.user_id, "token": token }))).into_response())
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, StatusCode> {
    if let Some(token) = session_token(&headers) {
        state.repo.delete_session(&hash_token(token)).await.map_err(internal)?;
    }
    Ok(([(header::SET_COOKIE, state.auth.cookie(SESSION_COOKIE, "", 0))], StatusCode::NO_CONTENT).into_response())
}

async fn me(State(state): State<AppState>, user: AuthUser) -> Result<Json<crate::model::UserSettings>, StatusCode> {
    state.repo.get_user(&user.0).await.map_err(internal)?.map(Json).ok_or(StatusCode::UNAUTHORIZED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_example() {
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn tokens_are_random_and_url_safe() {
        let (a, b) = (random_token(), random_token());
        assert_ne!(a, b);
        assert_eq!(a.len(), 43);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }

    #[test]
    fn reads_bearer_before_cookie() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, "a=1; tl_session=fromcookie; b=2".parse().unwrap());
        assert_eq!(session_token(&h), Some("fromcookie"));
        h.insert(header::AUTHORIZATION, "Bearer frombearer".parse().unwrap());
        assert_eq!(session_token(&h), Some("frombearer"));
        assert_eq!(cookie_value(&h, "tl_sess"), None, "exact name match only");
    }

    /// Mock X: /authorize records the PKCE challenge and redirects back with a code;
    /// /2/oauth2/token only accepts a verifier that hashes to it; /2/users/me returns @mike.
    async fn mock_x(listener: tokio::net::TcpListener) {
        use axum::{extract::Form, routing::{get, post}};
        type Codes = std::sync::Arc<Mutex<HashMap<String, (String, String)>>>;
        let codes: Codes = Default::default();
        let app = Router::new()
            .route("/authorize", get(|State(c): State<Codes>, Query(q): Query<HashMap<String, String>>| async move {
                assert_eq!((q["response_type"].as_str(), q["code_challenge_method"].as_str()), ("code", "S256"));
                assert_eq!(q["scope"], "tweet.read users.read");
                c.lock().unwrap().insert("code-1".into(), (q["code_challenge"].clone(), q["redirect_uri"].clone()));
                Redirect::to(&format!("{}?code=code-1&state={}", q["redirect_uri"], q["state"]))
            }))
            .route("/2/oauth2/token", post(|State(c): State<Codes>, Form(f): Form<HashMap<String, String>>| async move {
                let Some((challenge, redirect)) = c.lock().unwrap().remove(&f["code"]) else { return Err(StatusCode::BAD_REQUEST) };
                let ok = f["grant_type"] == "authorization_code" && f["client_id"] == "cid" && f["redirect_uri"] == redirect
                    && pkce_challenge(&f["code_verifier"]) == challenge;
                if !ok { return Err(StatusCode::BAD_REQUEST); }
                Ok(Json(json!({ "access_token": "at-1", "token_type": "bearer", "expires_in": 7200 })))
            }))
            .route("/2/users/me", get(|h: HeaderMap| async move {
                if h.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) != Some("Bearer at-1") { return Err(StatusCode::UNAUTHORIZED) }
                Ok(Json(json!({ "data": { "id": "42", "name": "Mike", "username": "mike" } })))
            }))
            .with_state(codes);
        axum::serve(listener, app).await.unwrap();
    }

    fn set_cookie(r: &reqwest::Response, name: &str) -> Option<String> {
        r.headers().get_all(header::SET_COOKIE).iter().filter_map(|v| v.to_str().ok())
            .find_map(|v| v.strip_prefix(&format!("{name}="))).map(|v| v.split(';').next().unwrap().to_string())
    }

    #[tokio::test]
    async fn sign_in_with_x_end_to_end() {
        let bind = || tokio::net::TcpListener::bind("127.0.0.1:0");
        let (app_l, x_l) = (bind().await.unwrap(), bind().await.unwrap());
        let (app_url, x_url) = (format!("http://{}", app_l.local_addr().unwrap()), format!("http://{}", x_l.local_addr().unwrap()));
        let x = XAuthConfig {
            client_id: "cid".into(), client_secret: None, redirect_uri: format!("{app_url}/auth/x/callback"),
            authorize_url: format!("{x_url}/authorize"), token_url: format!("{x_url}/2/oauth2/token"),
            api_base: x_url.clone(), scopes: "tweet.read users.read".into(),
        };
        let store = crate::store::Store::new();
        crate::seed::seed(&store);
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let state = AppState {
            repo: std::sync::Arc::new(store), lobbies: Default::default(),
            auth: std::sync::Arc::new(Auth::new(Some(x), false, http.clone())),
        };
        tokio::spawn(mock_x(x_l));
        tokio::spawn(async move { axum::serve(app_l, crate::app(state)).await.unwrap() });
        let c = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let loc = |r: &reqwest::Response| r.headers()[header::LOCATION].to_str().unwrap().to_string();

        // Browser: start sign-in, get sent to X, X sends it back with a code.
        let start = c.get(format!("{app_url}/auth/x/login")).send().await.unwrap();
        assert!(start.status().is_redirection());
        let state_cookie = set_cookie(&start, STATE_COOKIE).unwrap();
        let back = c.get(loc(&start)).send().await.unwrap();
        let callback = loc(&back);
        assert!(callback.starts_with(&format!("{app_url}/auth/x/callback?code=code-1&state=")));

        // The callback only completes in the browser that started it.
        let elsewhere = c.get(&callback).send().await.unwrap();
        assert_eq!(elsewhere.status(), StatusCode::BAD_REQUEST);
        let start = c.get(format!("{app_url}/auth/x/login")).send().await.unwrap();
        let state_cookie_2 = set_cookie(&start, STATE_COOKIE).unwrap();
        let callback = loc(&c.get(loc(&start)).send().await.unwrap());
        let done = c.get(&callback).header(header::COOKIE, format!("{STATE_COOKIE}={state_cookie_2}")).send().await.unwrap();
        assert_eq!(done.status(), StatusCode::SEE_OTHER);
        assert_ne!(state_cookie, state_cookie_2);
        let token = set_cookie(&done, SESSION_COOKIE).unwrap();
        assert_eq!(set_cookie(&done, STATE_COOKIE).as_deref(), Some(""), "state cookie cleared too");
        let cookie = format!("{SESSION_COOKIE}={token}");

        // Replaying the same callback fails: states are one-time.
        let replay = c.get(&callback).header(header::COOKIE, format!("{STATE_COOKIE}={state_cookie_2}")).send().await.unwrap();
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);

        // Signed in as @mike, by cookie or Bearer.
        let me: serde_json::Value = c.get(format!("{app_url}/api/me")).header(header::COOKIE, &cookie).send().await.unwrap().json().await.unwrap();
        assert_eq!((me["user_id"].as_str(), me["username"].as_str()), (Some("x-42"), Some("mike")));
        let me2 = c.get(format!("{app_url}/api/me")).bearer_auth(&token).send().await.unwrap();
        assert_eq!(me2.status(), StatusCode::OK);
        assert_eq!(c.get(format!("{app_url}/api/me")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(c.get(format!("{app_url}/api/me")).bearer_auth("forged").send().await.unwrap().status(), StatusCode::UNAUTHORIZED);

        // Identity comes from the session, not the request.
        let get = |path: &str| c.get(format!("{app_url}{path}")).bearer_auth(&token);
        assert_eq!(get("/api/users/demo/feed").send().await.unwrap().status(), StatusCode::FORBIDDEN);
        assert_eq!(get("/api/users/x-42/feed").send().await.unwrap().status(), StatusCode::OK);
        assert_eq!(c.get(format!("{app_url}/api/users/x-42/feed")).send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let post = |path: &str, body: serde_json::Value| c.post(format!("{app_url}{path}")).bearer_auth(&token).json(&body);
        assert_eq!(post("/api/lobbies", json!({ "id": "l1", "name": "L", "owner": "demo" })).send().await.unwrap().status(), StatusCode::FORBIDDEN);
        let created: serde_json::Value = post("/api/lobbies", json!({ "id": "l1", "name": "L" })).send().await.unwrap().json().await.unwrap();
        assert_eq!(created["owner"], "x-42");
        assert_eq!(post("/api/ingest", json!({ "user_id": "demo", "items": [] })).send().await.unwrap().status(), StatusCode::FORBIDDEN);
        let feed = get("/api/lobbies/l1/feed?user=demo").send().await.unwrap();
        assert_eq!(feed.status(), StatusCode::FORBIDDEN, "can't read a lobby feed as someone else");
        assert_eq!(get("/api/lobbies/l1/feed").send().await.unwrap().status(), StatusCode::OK);

        // Sign out ends the session; dev sign-in is off by default.
        assert_eq!(c.post(format!("{app_url}/auth/logout")).bearer_auth(&token).send().await.unwrap().status(), StatusCode::NO_CONTENT);
        assert_eq!(get("/api/me").send().await.unwrap().status(), StatusCode::UNAUTHORIZED);
        let dev = c.post(format!("{app_url}/auth/dev/login")).json(&json!({ "user_id": "demo" })).send().await.unwrap();
        assert_eq!(dev.status(), StatusCode::NOT_FOUND);

        // Declining on X's side.
        let declined = c.get(format!("{app_url}/auth/x/callback?error=access_denied&state=x")).send().await.unwrap();
        assert_eq!(declined.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn claim_rejects_other_users() {
        let me = AuthUser("alice".into());
        assert_eq!(me.claim(None), Ok("alice"));
        assert_eq!(me.claim(Some("alice")), Ok("alice"));
        assert_eq!(me.claim(Some("bob")), Err(StatusCode::FORBIDDEN));
    }
}
