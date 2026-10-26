//! OAuth 2.1 authorization server for MCP clients, so trend-liker can be added as a connector
//! (e.g. in claude.ai): dynamic client registration, authorization code + PKCE with a consent
//! page, and rotating refresh tokens. People sign in with X; access tokens are sessions.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::{
    extract::{Form, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use serde_json::json;

use crate::auth::{hash_token, pkce_challenge, random_token, session_token};
use crate::repo::OAuthClient;
use crate::{internal, AppState};

const ACCESS_TTL_SECS: i64 = 3600;
const REFRESH_TTL_DAYS: i64 = 30;
const REQUEST_TTL_SECS: i64 = 600;
const CODE_TTL_SECS: i64 = 300;
const MAX_PENDING: usize = 10_000;
pub const SCOPE: &str = "feeds";

pub struct OAuthConfig {
    /// This server's public base URL: the OAuth issuer.
    pub issuer: String,
    /// MCP server URLs tokens may be requested for (the `resource` parameter).
    pub resources: Vec<String>,
}

impl OAuthConfig {
    pub fn from_env() -> Self {
        let issuer = std::env::var("PUBLIC_URL").ok().filter(|v| !v.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:3000".into()).trim_end_matches('/').to_string();
        let resources = std::env::var("MCP_RESOURCE_URLS").ok().filter(|v| !v.is_empty())
            .map(|v| v.split(',').map(|r| r.trim().trim_end_matches('/').to_string()).collect())
            .unwrap_or_else(|| vec![format!("{issuer}/mcp")]);
        Self { issuer, resources }
    }
}

/// An authorization request waiting for the user's decision.
struct Request {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    state: Option<String>,
    created: DateTime<Utc>,
    csrf: String,
    /// The first signed-in user to open the consent page; only they can answer it.
    viewer: Option<String>,
}

/// An authorization code, redeemable once.
struct Grant { client_id: String, redirect_uri: String, challenge: String, user: String, created: DateTime<Utc> }

pub struct OAuth {
    pub config: OAuthConfig,
    requests: Mutex<HashMap<String, Request>>,
    codes: Mutex<HashMap<String, Grant>>,
}

impl OAuth {
    pub fn new(config: OAuthConfig) -> Self {
        Self { config, requests: Mutex::new(HashMap::new()), codes: Mutex::new(HashMap::new()) }
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/.well-known/oauth-authorization-server", get(metadata))
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/consent", get(consent_page).post(consent_decision))
        .route("/oauth/token", post(token))
}

async fn metadata(State(state): State<AppState>) -> Json<serde_json::Value> {
    let i = &state.oauth.config.issuer;
    Json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/oauth/authorize"),
        "token_endpoint": format!("{i}/oauth/token"),
        "registration_endpoint": format!("{i}/oauth/register"),
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256"],
        "token_endpoint_auth_methods_supported": ["none"],
        "scopes_supported": [SCOPE],
        "authorization_response_iss_parameter_supported": true,
    }))
}

/// https anywhere, or http only to this machine (native apps); no fragments.
pub fn valid_redirect_uri(uri: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(uri) else { return false };
    if uri.len() > 2000 || u.fragment().is_some() || !u.username().is_empty() || u.password().is_some() { return false; }
    match u.scheme() {
        "https" => u.host_str().is_some(),
        "http" => matches!(u.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")),
        _ => false,
    }
}

#[derive(Deserialize)]
struct Registration {
    redirect_uris: Vec<String>,
    #[serde(default)]
    client_name: Option<String>,
}

fn oauth_error(status: StatusCode, error: &str, description: &str) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(json!({ "error": error, "error_description": description }))).into_response()
}

/// RFC 7591 dynamic registration. Every client is public (PKCE, no secret).
async fn register(State(state): State<AppState>, Json(r): Json<Registration>) -> Result<Response, Response> {
    let bad = |d: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_redirect_uri", d);
    if r.redirect_uris.is_empty() || r.redirect_uris.len() > 5 { return Err(bad("register 1 to 5 redirect_uris")); }
    if let Some(u) = r.redirect_uris.iter().find(|u| !valid_redirect_uri(u)) {
        return Err(bad(&format!("not allowed: {u} (https, or http to localhost)")));
    }
    let name: String = r.client_name.unwrap_or_default().chars().filter(|c| !c.is_control()).take(100).collect();
    let client = OAuthClient { client_id: random_token(), client_name: name, redirect_uris: r.redirect_uris };
    state.repo.register_client(&client).await.map_err(|e| internal(e).into_response())?;
    Ok((StatusCode::CREATED, Json(json!({
        "client_id": client.client_id,
        "client_name": client.client_name,
        "redirect_uris": client.redirect_uris,
        "client_id_issued_at": Utc::now().timestamp(),
        "token_endpoint_auth_method": "none",
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
    }))).into_response())
}

/// `uri` with extra query parameters, keeping any it already has.
fn with_params(uri: &str, params: &[(&str, &str)]) -> String {
    let mut u = reqwest::Url::parse(uri).expect("validated at registration");
    u.query_pairs_mut().extend_pairs(params);
    u.to_string()
}

#[derive(Deserialize)]
struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    resource: Option<String>,
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(r#"<!DOCTYPE html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{t}</title><style>
:root{{--bg:#f6f6f4;--card:#fff;--ink:#16181d;--muted:#6b7080;--line:#e3e3df;--accent:#2a5bd7}}
@media (prefers-color-scheme:dark){{:root{{--bg:#121316;--card:#1b1d22;--ink:#e9eaee;--muted:#9097a6;--line:#2b2e35;--accent:#7aa2ff}}}}
body{{font:15px/1.5 system-ui,sans-serif;max-width:520px;margin:40px auto;padding:0 16px;background:var(--bg);color:var(--ink)}}
.card{{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:20px}}h1{{font-size:20px;margin:0 0 12px}}
.muted{{color:var(--muted);font-size:13px}}ul{{padding-left:20px}}button{{font:inherit;padding:8px 16px;border-radius:8px;border:1px solid var(--line);background:var(--card);color:var(--ink);cursor:pointer;margin-right:8px}}
button.primary{{background:var(--accent);border-color:var(--accent);color:#fff}}code{{font-size:13px}}
</style></head><body><div class="card"><h1>{t}</h1>{body}</div></body></html>"#, t = esc(title));
    // The consent page must never be framed (clickjacking).
    (status, [(header::X_FRAME_OPTIONS, "DENY"), (header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'"),
              (header::CACHE_CONTROL, "no-store")], Html(html)).into_response()
}

pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

async fn signed_in(state: &AppState, headers: &HeaderMap) -> Result<Option<String>, StatusCode> {
    let Some(t) = session_token(headers) else { return Ok(None) };
    state.repo.session_user(&hash_token(t), Utc::now()).await.map_err(internal)
}

async fn authorize(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<AuthorizeQuery>) -> Result<Response, Response> {
    let stop = |msg: &str| page(StatusCode::BAD_REQUEST, "Can't connect", &format!("<p>{}</p>", esc(msg)));
    // Until the client and redirect are verified, never redirect anywhere.
    let (Some(client_id), Some(redirect_uri)) = (q.client_id, q.redirect_uri) else { return Err(stop("missing client_id or redirect_uri")) };
    let client = state.repo.get_client(&client_id).await.map_err(|e| internal(e).into_response())?
        .ok_or_else(|| stop("this app isn't registered"))?;
    if !client.redirect_uris.contains(&redirect_uri) { return Err(stop("redirect_uri doesn't match what the app registered")); }

    let state_param = q.state.clone();
    let fail = |error: &str| {
        let mut p = vec![("error", error)];
        if let Some(s) = &state_param { p.push(("state", s)); }
        Redirect::to(&with_params(&redirect_uri, &p)).into_response()
    };
    if q.response_type.as_deref() != Some("code") { return Err(fail("unsupported_response_type")); }
    let Some(challenge) = q.code_challenge.filter(|c| (43..=128).contains(&c.len())) else { return Err(fail("invalid_request")) };
    if q.code_challenge_method.as_deref() != Some("S256") { return Err(fail("invalid_request")); }
    if let Some(r) = &q.resource {
        if !state.oauth.config.resources.contains(&r.trim_end_matches('/').to_string()) { return Err(fail("invalid_target")); }
    }
    if state.auth.x.is_none() && !state.auth.dev_login {
        return Err(page(StatusCode::SERVICE_UNAVAILABLE, "Sign-in unavailable", "<p>This server has no sign-in configured.</p>"));
    }

    let id = random_token();
    {
        let mut reqs = state.oauth.requests.lock().unwrap();
        let cutoff = Utc::now() - Duration::seconds(REQUEST_TTL_SECS);
        reqs.retain(|_, r| r.created > cutoff);
        if reqs.len() >= MAX_PENDING { return Err(StatusCode::SERVICE_UNAVAILABLE.into_response()); }
        reqs.insert(id.clone(), Request {
            client_id, redirect_uri: redirect_uri.clone(), challenge, state: q.state, created: Utc::now(),
            csrf: random_token(), viewer: None,
        });
    }
    let consent = format!("/oauth/consent?req={id}");
    if signed_in(&state, &headers).await.map_err(IntoResponse::into_response)?.is_some() {
        return Ok(Redirect::to(&consent).into_response());
    }
    Ok(Redirect::to(&with_params("http://x/auth/x/login", &[("next", &consent)]).replacen("http://x", "", 1)).into_response())
}

#[derive(Deserialize)]
struct ConsentQuery { req: String }

async fn consent_page(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<ConsentQuery>) -> Result<Response, Response> {
    let Some(user) = signed_in(&state, &headers).await.map_err(IntoResponse::into_response)? else {
        let next = format!("/oauth/consent?req={}", q.req);
        return Ok(Redirect::to(&with_params("http://x/auth/x/login", &[("next", &next)]).replacen("http://x", "", 1)).into_response());
    };
    let (client_id, redirect_uri, csrf) = {
        let mut reqs = state.oauth.requests.lock().unwrap();
        let Some(r) = reqs.get_mut(&q.req).filter(|r| r.created > Utc::now() - Duration::seconds(REQUEST_TTL_SECS)) else {
            return Err(page(StatusCode::BAD_REQUEST, "Request expired", "<p>Go back to the app and connect again.</p>"));
        };
        if r.viewer.get_or_insert_with(|| user.clone()) != &user {
            return Err(page(StatusCode::FORBIDDEN, "Not your request", "<p>This connection request belongs to someone else.</p>"));
        }
        (r.client_id.clone(), r.redirect_uri.clone(), r.csrf.clone())
    };
    let client = state.repo.get_client(&client_id).await.map_err(|e| internal(e).into_response())?
        .ok_or(StatusCode::BAD_REQUEST.into_response())?;
    let who = state.repo.get_user(&user).await.map_err(|e| internal(e).into_response())?
        .and_then(|u| u.username.map(|n| format!("@{n}"))).unwrap_or(user.clone());
    let host = reqwest::Url::parse(&redirect_uri).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let name = if client.client_name.is_empty() { "An app".to_string() } else { client.client_name.clone() };
    let body = format!(r#"<p><b>{name}</b> wants to use trend-liker as <b>{who}</b>.</p>
<p class="muted">After you approve, you'll be sent back to <code>{host}</code>.</p>
<p>It will be able to:</p><ul>
<li>read your feeds, lobbies and your X home timeline (the posts X shows you)</li>
<li>join lobbies, vote, and share posts as you, and change your publishing setting</li></ul>
<p class="muted">You can disconnect X at any time; the app's access expires unless it keeps refreshing it.</p>
<form method="post" action="/oauth/consent"><input type="hidden" name="req" value="{req}"><input type="hidden" name="csrf" value="{csrf}">
<button class="primary" name="decision" value="approve">Approve</button><button name="decision" value="deny">Deny</button></form>"#,
        name = esc(&name), who = esc(&who), host = esc(&host), req = esc(&q.req), csrf = esc(&csrf));
    Ok(page(StatusCode::OK, "Connect to trend-liker", &body))
}

#[derive(Deserialize)]
struct Decision { req: String, csrf: String, decision: String }

async fn consent_decision(State(state): State<AppState>, headers: HeaderMap, Form(d): Form<Decision>) -> Result<Response, Response> {
    let forbidden = || page(StatusCode::FORBIDDEN, "Can't continue", "<p>Start the connection again from the app.</p>");
    let user = signed_in(&state, &headers).await.map_err(IntoResponse::into_response)?.ok_or_else(forbidden)?;
    let req = state.oauth.requests.lock().unwrap().remove(&d.req).ok_or_else(forbidden)?;
    // Same browser session that viewed the page, the token rendered on it, and still fresh.
    if req.viewer.as_deref() != Some(user.as_str()) || req.csrf != d.csrf
        || req.created <= Utc::now() - Duration::seconds(REQUEST_TTL_SECS) {
        return Err(forbidden());
    }
    let mut params: Vec<(&str, String)> = vec![("iss", state.oauth.config.issuer.clone())];
    if d.decision == "approve" {
        let code = random_token();
        state.oauth.codes.lock().unwrap().insert(code.clone(), Grant {
            client_id: req.client_id, redirect_uri: req.redirect_uri.clone(), challenge: req.challenge, user, created: Utc::now(),
        });
        params.push(("code", code));
    } else {
        params.push(("error", "access_denied".into()));
    }
    if let Some(s) = req.state { params.push(("state", s)); }
    let refs: Vec<(&str, &str)> = params.iter().map(|(k, v)| (*k, v.as_str())).collect();
    Ok(Redirect::to(&with_params(&req.redirect_uri, &refs)).into_response())
}

#[derive(Deserialize)]
struct TokenRequest {
    grant_type: String,
    code: Option<String>,
    redirect_uri: Option<String>,
    client_id: Option<String>,
    code_verifier: Option<String>,
    refresh_token: Option<String>,
}

async fn issue(state: &AppState, user: &str, client_id: &str) -> Result<Response, Response> {
    let (access, refresh) = (random_token(), random_token());
    let now = Utc::now();
    state.repo.create_session(&hash_token(&access), user, now + Duration::seconds(ACCESS_TTL_SECS)).await
        .map_err(|e| internal(e).into_response())?;
    state.repo.create_refresh_token(&hash_token(&refresh), client_id, user, now + Duration::days(REFRESH_TTL_DAYS)).await
        .map_err(|e| internal(e).into_response())?;
    Ok(([(header::CACHE_CONTROL, "no-store"), (header::PRAGMA, "no-cache")], Json(json!({
        "access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TTL_SECS,
        "refresh_token": refresh, "scope": SCOPE,
    }))).into_response())
}

async fn token(State(state): State<AppState>, Form(t): Form<TokenRequest>) -> Result<Response, Response> {
    let invalid = |d: &str| oauth_error(StatusCode::BAD_REQUEST, "invalid_grant", d);
    let client_id = t.client_id.ok_or_else(|| oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "client_id required"))?;
    if state.repo.get_client(&client_id).await.map_err(|e| internal(e).into_response())?.is_none() {
        return Err(oauth_error(StatusCode::UNAUTHORIZED, "invalid_client", "unknown client"));
    }
    match t.grant_type.as_str() {
        "authorization_code" => {
            let code = t.code.ok_or_else(|| invalid("code required"))?;
            let grant = state.oauth.codes.lock().unwrap().remove(&code).ok_or_else(|| invalid("unknown or used code"))?;
            let verifier = t.code_verifier.unwrap_or_default();
            let ok = grant.created > Utc::now() - Duration::seconds(CODE_TTL_SECS)
                && grant.client_id == client_id
                && t.redirect_uri.as_deref() == Some(grant.redirect_uri.as_str())
                && (43..=128).contains(&verifier.len())
                && pkce_challenge(&verifier) == grant.challenge;
            if !ok { return Err(invalid("code, client, redirect_uri or code_verifier don't match")); }
            issue(&state, &grant.user, &client_id).await
        }
        "refresh_token" => {
            let rt = t.refresh_token.ok_or_else(|| invalid("refresh_token required"))?;
            let (owner, user) = state.repo.take_refresh_token(&hash_token(&rt), Utc::now()).await
                .map_err(|e| internal(e).into_response())?.ok_or_else(|| invalid("unknown, used or expired refresh token"))?;
            if owner != client_id { return Err(invalid("refresh token belongs to another client")); }
            issue(&state, &user, &client_id).await
        }
        _ => Err(oauth_error(StatusCode::BAD_REQUEST, "unsupported_grant_type", "use authorization_code or refresh_token")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uri_rules() {
        for ok in ["https://claude.ai/api/mcp/auth_callback", "http://127.0.0.1:33418/callback", "http://localhost/cb"] {
            assert!(valid_redirect_uri(ok), "{ok}");
        }
        for bad in ["http://claude.ai/cb", "https://claude.ai/cb#frag", "javascript:alert(1)", "myapp://cb",
                    "https://user:pw@claude.ai/cb", "not a url"] {
            assert!(!valid_redirect_uri(bad), "{bad}");
        }
    }

    #[test]
    fn params_are_added_and_encoded() {
        assert_eq!(with_params("https://a.example/cb?x=1", &[("code", "a b"), ("state", "s&t")]),
                   "https://a.example/cb?x=1&code=a+b&state=s%26t");
    }

    /// Mock X: sign-in that issues short-lived tokens plus refresh tokens, a home timeline that
    /// requires a current token, and revocation. Counts refreshes and revocations.
    #[derive(Clone, Default)]
    struct MockX { inner: std::sync::Arc<Mutex<MockState>> }
    #[derive(Default)]
    struct MockState { challenges: HashMap<String, String>, access: Vec<String>, refresh: Vec<String>, refreshes: usize, revoked: usize, n: usize }

    async fn mock_x(listener: tokio::net::TcpListener, mock: MockX) {
        use axum::extract::Path;
        let app = Router::new()
            .route("/authorize", get(|State(m): State<MockX>, Query(q): Query<HashMap<String, String>>| async move {
                m.inner.lock().unwrap().challenges.insert("xcode".into(), q["code_challenge"].clone());
                assert!(q["scope"].contains("offline.access"));
                Redirect::to(&format!("{}?code=xcode&state={}", q["redirect_uri"], q["state"]))
            }))
            .route("/2/oauth2/token", post(|State(m): State<MockX>, Form(f): Form<HashMap<String, String>>| async move {
                let mut s = m.inner.lock().unwrap();
                match f["grant_type"].as_str() {
                    "authorization_code" => {
                        let ch = s.challenges.remove(&f["code"]).ok_or(StatusCode::BAD_REQUEST)?;
                        if pkce_challenge(&f["code_verifier"]) != ch { return Err(StatusCode::BAD_REQUEST); }
                    }
                    "refresh_token" => {
                        let pos = s.refresh.iter().position(|r| r == &f["refresh_token"]).ok_or(StatusCode::BAD_REQUEST)?;
                        s.refresh.remove(pos); // rotation: each refresh token works once
                        s.refreshes += 1;
                    }
                    _ => return Err(StatusCode::BAD_REQUEST),
                }
                s.n += 1;
                let (a, r) = (format!("xa{}", s.n), format!("xr{}", s.n));
                s.access.push(a.clone());
                s.refresh.push(r.clone());
                // expires_in 1s: the server must refresh before its next timeline call.
                Ok(Json(json!({ "access_token": a, "refresh_token": r, "expires_in": 1, "scope": "tweet.read users.read offline.access" })))
            }))
            .route("/2/users/me", get(|State(m): State<MockX>, h: HeaderMap| async move {
                let t = h[header::AUTHORIZATION].to_str().unwrap().trim_start_matches("Bearer ").to_string();
                if !m.inner.lock().unwrap().access.contains(&t) { return Err(StatusCode::UNAUTHORIZED); }
                Ok(Json(json!({ "data": { "id": "42", "username": "mike" } })))
            }))
            .route("/2/users/{id}/timelines/reverse_chronological", get(|State(m): State<MockX>, Path(id): Path<String>, h: HeaderMap,
                                                                        Query(q): Query<HashMap<String, String>>| async move {
                let t = h[header::AUTHORIZATION].to_str().unwrap().trim_start_matches("Bearer ").to_string();
                let s = m.inner.lock().unwrap();
                // Only the newest access token is valid: older ones were superseded by a refresh.
                if s.access.last() != Some(&t) || id != "42" { return Err(StatusCode::UNAUTHORIZED); }
                let start: usize = q.get("pagination_token").map(|p| p.parse().unwrap()).unwrap_or(0);
                let data: Vec<_> = (start..(start + 3).min(7)).map(|i| json!({
                    "id": format!("t{i}"), "text": format!("post {i}"), "author_id": if i % 2 == 0 { "a1" } else { "a2" },
                    "created_at": "2026-10-26T00:00:00Z", "public_metrics": { "like_count": i, "retweet_count": 0, "reply_count": 0 } })).collect();
                let mut meta = json!({ "result_count": data.len() });
                if start + 3 < 7 { meta["next_token"] = json!((start + 3).to_string()); }
                Ok(Json(json!({ "data": data, "meta": meta, "includes": { "users": [
                    { "id": "a1", "username": "alice", "protected": false }, { "id": "a2", "username": "bob", "protected": true }] } })))
            }))
            .route("/2/oauth2/revoke", post(|State(m): State<MockX>| async move { m.inner.lock().unwrap().revoked += 1; StatusCode::OK }))
            .with_state(mock);
        axum::serve(listener, app).await.unwrap();
    }

    #[tokio::test]
    async fn connector_sign_in_end_to_end() {
        let bind = || tokio::net::TcpListener::bind("127.0.0.1:0");
        let (app_l, x_l) = (bind().await.unwrap(), bind().await.unwrap());
        let (app, xu) = (format!("http://{}", app_l.local_addr().unwrap()), format!("http://{}", x_l.local_addr().unwrap()));
        let x = crate::auth::XAuthConfig {
            client_id: "cid".into(), client_secret: None, redirect_uri: format!("{app}/auth/x/callback"),
            authorize_url: format!("{xu}/authorize"), token_url: format!("{xu}/2/oauth2/token"), api_base: xu.clone(),
            revoke_url: Some(format!("{xu}/2/oauth2/revoke")), scopes: "tweet.read users.read offline.access".into(),
        };
        let store = crate::store::Store::new();
        crate::seed::seed(&store);
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let state = AppState {
            repo: std::sync::Arc::new(store.clone()), lobbies: Default::default(),
            auth: std::sync::Arc::new(crate::auth::Auth::new(Some(x), false, http)
                .with_cipher(Some(crate::xapi::TokenCipher::new(&[9u8; 32]).unwrap()))),
            oauth: std::sync::Arc::new(OAuth::new(OAuthConfig { issuer: app.clone(), resources: vec!["https://mcp.example/mcp".into()] })),
        };
        let mock = MockX::default();
        tokio::spawn(mock_x(x_l, mock.clone()));
        tokio::spawn(async move { axum::serve(app_l, crate::app(state)).await.unwrap() });
        let c = reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        let loc = |r: &reqwest::Response| r.headers()[header::LOCATION].to_str().unwrap().to_string();
        let cookies = |r: &reqwest::Response| r.headers().get_all(header::SET_COOKIE).iter()
            .map(|v| v.to_str().unwrap().split(';').next().unwrap().to_string()).filter(|c| !c.ends_with('=')).collect::<Vec<_>>().join("; ");

        // 1. Discovery and dynamic registration (what claude.ai does when you add the connector).
        let meta: serde_json::Value = c.get(format!("{app}/.well-known/oauth-authorization-server")).send().await.unwrap().json().await.unwrap();
        assert_eq!(meta["issuer"], app.as_str());
        assert_eq!(meta["code_challenge_methods_supported"], json!(["S256"]));
        let cb = "https://claude.example/api/mcp/auth_callback";
        let bad = c.post(format!("{app}/oauth/register")).json(&json!({ "redirect_uris": ["http://evil.example/cb"] })).send().await.unwrap();
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
        let reg: serde_json::Value = c.post(format!("{app}/oauth/register"))
            .json(&json!({ "redirect_uris": [cb], "client_name": "<script>alert(1)</script>Claude" })).send().await.unwrap().json().await.unwrap();
        let client_id = reg["client_id"].as_str().unwrap().to_string();

        // 2. Authorize: bad client or redirect is refused outright (no redirect); a good one goes to X sign-in.
        let verifier = "v".repeat(64);
        let authz = |redirect: &str, resource: &str| format!("{app}/oauth/authorize?response_type=code&client_id={client_id}&redirect_uri={}&code_challenge={}&code_challenge_method=S256&state=st1&resource={}",
            urlencode(redirect), pkce_challenge(&verifier), urlencode(resource));
        let r = c.get(authz("https://evil.example/cb", "https://mcp.example/mcp")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let r = c.get(authz(cb, "https://other.example/mcp")).send().await.unwrap();
        assert!(loc(&r).starts_with(cb) && loc(&r).contains("error=invalid_target"));
        let r = c.get(authz(cb, "https://mcp.example/mcp")).send().await.unwrap();
        let to_login = loc(&r);
        assert!(to_login.starts_with("/auth/x/login?next=%2Foauth%2Fconsent%3Freq%3D"), "{to_login}");

        // 3. Sign in with X, which returns to the consent page.
        let start = c.get(format!("{app}{to_login}")).send().await.unwrap();
        let state_cookie = cookies(&start);
        let back = c.get(loc(&start)).send().await.unwrap();
        let signed = c.get(loc(&back)).header(header::COOKIE, &state_cookie).send().await.unwrap();
        let session = cookies(&signed);
        let consent_path = loc(&signed);
        assert!(consent_path.starts_with("/oauth/consent?req="), "{consent_path}");
        let page = c.get(format!("{app}{consent_path}")).header(header::COOKIE, &session).send().await.unwrap();
        assert_eq!(page.headers()[header::X_FRAME_OPTIONS], "DENY");
        let html = page.text().await.unwrap();
        assert!(html.contains("&lt;script&gt;alert(1)&lt;/script&gt;Claude") && !html.contains("<script>alert"), "client name escaped");
        assert!(html.contains("@mike") && html.contains("claude.example"));
        let field = |name: &str| html.split(&format!(r#"name="{name}" value=""#)).nth(1).unwrap().split('"').next().unwrap().to_string();
        let (req, csrf) = (field("req"), field("csrf"));

        // Someone else (here: the demo user's session) can't view or answer mike's request.
        use crate::repo::Repo;
        store.create_session(&hash_token("demo-session"), "demo", Utc::now() + Duration::hours(1)).await.unwrap();
        let demo = format!("{}=demo-session", crate::auth::SESSION_COOKIE);
        let hijack = c.get(format!("{app}{consent_path}")).header(header::COOKIE, &demo).send().await.unwrap();
        assert_eq!(hijack.status(), StatusCode::FORBIDDEN);
        let hijack = c.post(format!("{app}/oauth/consent")).header(header::COOKIE, &demo)
            .form(&[("req", req.as_str()), ("csrf", csrf.as_str()), ("decision", "approve")]).send().await.unwrap();
        assert_eq!(hijack.status(), StatusCode::FORBIDDEN);
        // A wrong CSRF token is refused too.
        let r = c.get(authz(cb, "https://mcp.example/mcp")).header(header::COOKIE, &session).send().await.unwrap();
        let html = c.get(format!("{app}{}", loc(&r))).header(header::COOKIE, &session).send().await.unwrap().text().await.unwrap();
        let req = html.split(r#"name="req" value=""#).nth(1).unwrap().split('"').next().unwrap().to_string();
        let wrong = c.post(format!("{app}/oauth/consent")).header(header::COOKIE, &session)
            .form(&[("req", req.as_str()), ("csrf", "nope"), ("decision", "approve")]).send().await.unwrap();
        assert_eq!(wrong.status(), StatusCode::FORBIDDEN);
        // (the failed attempt consumed the request: start over, as a user would)
        let r = c.get(authz(cb, "https://mcp.example/mcp")).header(header::COOKIE, &session).send().await.unwrap();
        let consent_path = loc(&r);
        assert!(consent_path.starts_with("/oauth/consent?req="), "already signed in: straight to consent");
        let html = c.get(format!("{app}{consent_path}")).header(header::COOKIE, &session).send().await.unwrap().text().await.unwrap();
        let field = |name: &str| html.split(&format!(r#"name="{name}" value=""#)).nth(1).unwrap().split('"').next().unwrap().to_string();
        let (req, csrf) = (field("req"), field("csrf"));
        let done = c.post(format!("{app}/oauth/consent")).header(header::COOKIE, &session)
            .form(&[("req", req.as_str()), ("csrf", csrf.as_str()), ("decision", "approve")]).send().await.unwrap();
        let back_to_client = loc(&done);
        assert!(back_to_client.starts_with(&format!("{cb}?iss=")) && back_to_client.contains("state=st1"));
        let code = reqwest::Url::parse(&back_to_client).unwrap().query_pairs().find(|(k, _)| k == "code").unwrap().1.to_string();

        // 4. Token exchange: PKCE enforced, codes are single-use.
        let exchange = |verifier: String| c.post(format!("{app}/oauth/token")).form(&[
            ("grant_type", "authorization_code".to_string()), ("code", code.clone()), ("redirect_uri", cb.to_string()),
            ("client_id", client_id.clone()), ("code_verifier", verifier)]).send();
        let wrong_pkce = exchange("w".repeat(64)).await.unwrap();
        assert_eq!(wrong_pkce.status(), StatusCode::BAD_REQUEST);
        // The wrong verifier burned the code (single-use); get a fresh one.
        let r = c.get(authz(cb, "https://mcp.example/mcp")).header(header::COOKIE, &session).send().await.unwrap();
        let html = c.get(format!("{app}{}", loc(&r))).header(header::COOKIE, &session).send().await.unwrap().text().await.unwrap();
        let field = |name: &str| html.split(&format!(r#"name="{name}" value=""#)).nth(1).unwrap().split('"').next().unwrap().to_string();
        let done = c.post(format!("{app}/oauth/consent")).header(header::COOKIE, &session)
            .form(&[("req", field("req").as_str()), ("csrf", field("csrf").as_str()), ("decision", "approve")]).send().await.unwrap();
        let code = reqwest::Url::parse(&loc(&done)).unwrap().query_pairs().find(|(k, _)| k == "code").unwrap().1.to_string();
        let tok_req = |code: &str| c.post(format!("{app}/oauth/token")).form(&[
            ("grant_type", "authorization_code"), ("code", code), ("redirect_uri", cb), ("client_id", client_id.as_str()),
            ("code_verifier", verifier.as_str())]).send();
        let tokens: serde_json::Value = tok_req(&code).await.unwrap().json().await.unwrap();
        assert_eq!(tok_req(&code).await.unwrap().status(), StatusCode::BAD_REQUEST, "code reuse refused");
        let access = tokens["access_token"].as_str().unwrap().to_string();
        assert_eq!(tokens["token_type"], "Bearer");

        // 5. The token acts as @mike, and reads the X timeline (refreshing X's 1-second token on the way).
        let me: serde_json::Value = c.get(format!("{app}/api/me")).bearer_auth(&access).send().await.unwrap().json().await.unwrap();
        assert_eq!(me["user_id"], "x-42");
        let x_status: serde_json::Value = c.get(format!("{app}/api/me/x")).bearer_auth(&access).send().await.unwrap().json().await.unwrap();
        assert_eq!(x_status["connected"], true);
        let stored = store.x_tokens.read().unwrap().get("x-42").cloned().unwrap();
        assert!(!String::from_utf8_lossy(&stored.access_token).contains("xa1"), "X token encrypted at rest");
        let tl: serde_json::Value = c.get(format!("{app}/api/me/x/timeline?limit=5")).bearer_auth(&access).send().await.unwrap().json().await.unwrap();
        assert_eq!(tl["count"], 5, "paged across X's pages");
        assert_eq!(tl["posts"][1]["author_username"], "bob");
        assert_eq!(tl["posts"][1]["protected"], true);
        assert_eq!(mock.inner.lock().unwrap().refreshes, 1, "expired X token refreshed once");
        let tl: serde_json::Value = c.get(format!("{app}/api/me/x/timeline?limit=50")).bearer_auth(&access).send().await.unwrap().json().await.unwrap();
        assert_eq!(tl["count"], 7, "stops at the end of the timeline");

        // 6. Refresh tokens rotate; the old one stops working.
        let refresh = |rt: String| c.post(format!("{app}/oauth/token"))
            .form(&[("grant_type", "refresh_token".to_string()), ("refresh_token", rt), ("client_id", client_id.clone())]).send();
        let rt1 = tokens["refresh_token"].as_str().unwrap().to_string();
        let t2: serde_json::Value = refresh(rt1.clone()).await.unwrap().json().await.unwrap();
        assert_ne!(t2["access_token"], tokens["access_token"]);
        assert_eq!(refresh(rt1).await.unwrap().status(), StatusCode::BAD_REQUEST, "used refresh token refused");
        let other = c.post(format!("{app}/oauth/token")).form(&[("grant_type", "refresh_token"),
            ("refresh_token", t2["refresh_token"].as_str().unwrap()), ("client_id", "someone-else")]).send().await.unwrap();
        assert_eq!(other.status(), StatusCode::UNAUTHORIZED, "unknown client");

        // 7. Disconnecting X revokes at X and stops timeline access.
        let a2 = t2["access_token"].as_str().unwrap();
        let d: serde_json::Value = c.delete(format!("{app}/api/me/x")).bearer_auth(a2).send().await.unwrap().json().await.unwrap();
        assert_eq!(d["disconnected"], true);
        assert_eq!(mock.inner.lock().unwrap().revoked, 1);
        let tl = c.get(format!("{app}/api/me/x/timeline")).bearer_auth(a2).send().await.unwrap();
        assert_eq!(tl.status(), StatusCode::CONFLICT);
        let body: serde_json::Value = tl.json().await.unwrap();
        assert_eq!(body["error"], "x_not_connected");

        // Declining sends the client an error, not a code.
        let r = c.get(authz(cb, "https://mcp.example/mcp")).header(header::COOKIE, &session).send().await.unwrap();
        let html = c.get(format!("{app}{}", loc(&r))).header(header::COOKIE, &session).send().await.unwrap().text().await.unwrap();
        let field = |name: &str| html.split(&format!(r#"name="{name}" value=""#)).nth(1).unwrap().split('"').next().unwrap().to_string();
        let denied = c.post(format!("{app}/oauth/consent")).header(header::COOKIE, &session)
            .form(&[("req", field("req").as_str()), ("csrf", field("csrf").as_str()), ("decision", "deny")]).send().await.unwrap();
        assert!(loc(&denied).contains("error=access_denied") && !loc(&denied).contains("code="));
    }

    fn urlencode(s: &str) -> String {
        reqwest::Url::parse_with_params("http://x/", &[("v", s)]).unwrap().query().unwrap()[2..].to_string()
    }

    #[test]
    fn escapes_html() {
        assert_eq!(esc(r#"<b onclick="x">'&"#), "&lt;b onclick=&quot;x&quot;&gt;&#39;&amp;");
    }
}
