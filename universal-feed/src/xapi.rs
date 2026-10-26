//! A signed-in user's own X data: their stored X tokens (encrypted at rest, refreshed as
//! needed) and their home timeline, i.e. the posts X is feeding them.

use aes_gcm::{aead::{Aead, KeyInit, Payload}, Aes256Gcm, Nonce};
use axum::{
    extract::{Query, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{auth::AuthUser, internal, repo::XTokens, AppState};

const MAX_TIMELINE: usize = 800;
const REFRESH_EARLY_SECS: i64 = 60;

/// AES-256-GCM with a random nonce per message; the user id is bound in as associated data,
/// so a ciphertext copied onto another user's row won't decrypt.
pub struct TokenCipher(Aes256Gcm);

impl TokenCipher {
    /// TOKEN_ENCRYPTION_KEY: 32 bytes, base64. Without it X tokens aren't stored at all.
    pub fn from_env() -> Result<Option<Self>, String> {
        let Some(k) = std::env::var("TOKEN_ENCRYPTION_KEY").ok().filter(|v| !v.is_empty()) else { return Ok(None) };
        let key = STANDARD.decode(k.trim()).map_err(|_| "TOKEN_ENCRYPTION_KEY must be base64".to_string())?;
        Self::new(&key).map(Some)
    }

    pub fn new(key: &[u8]) -> Result<Self, String> {
        if key.len() != 32 { return Err("TOKEN_ENCRYPTION_KEY must decode to 32 bytes".into()); }
        Ok(Self(Aes256Gcm::new_from_slice(key).expect("length checked")))
    }

    pub fn seal(&self, user: &str, plain: &str) -> Vec<u8> {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).expect("OS randomness unavailable");
        let ct = self.0.encrypt(Nonce::from_slice(&nonce), Payload { msg: plain.as_bytes(), aad: user.as_bytes() })
            .expect("AES-GCM encryption does not fail for these sizes");
        [nonce.as_slice(), &ct].concat()
    }

    pub fn open(&self, user: &str, sealed: &[u8]) -> Option<String> {
        if sealed.len() < 12 { return None; }
        let (nonce, ct) = sealed.split_at(12);
        let plain = self.0.decrypt(Nonce::from_slice(nonce), Payload { msg: ct, aad: user.as_bytes() }).ok()?;
        String::from_utf8(plain).ok()
    }
}

/// What X's token endpoint returns (for both the first exchange and refreshes).
#[derive(Deserialize)]
pub struct XTokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
    pub scope: Option<String>,
}

/// Encrypt and store tokens from X. A no-op (returns false) when no encryption key is set.
pub async fn store_tokens(state: &AppState, user: &str, t: &XTokenResponse) -> anyhow::Result<bool> {
    let Some(cipher) = &state.auth.cipher else { return Ok(false) };
    let tokens = XTokens {
        access_token: cipher.seal(user, &t.access_token),
        refresh_token: t.refresh_token.as_deref().map(|r| cipher.seal(user, r)),
        expires_at: Utc::now() + Duration::seconds(t.expires_in.unwrap_or(7200)),
        scope: t.scope.clone().unwrap_or_default(),
    };
    state.repo.put_x_tokens(user, &tokens).await?;
    Ok(true)
}

enum XError {
    /// No usable X connection: sign in with X again.
    NotConnected(&'static str),
    RateLimited(Option<String>),
    Upstream(String),
    Internal(anyhow::Error),
}

impl IntoResponse for XError {
    fn into_response(self) -> Response {
        match self {
            XError::NotConnected(why) => (StatusCode::CONFLICT, Json(json!({
                "error": "x_not_connected", "detail": why, "fix": "sign in with X again (/auth/x/login)" }))).into_response(),
            XError::RateLimited(retry) => {
                let mut r = (StatusCode::TOO_MANY_REQUESTS, Json(json!({ "error": "x_rate_limited" }))).into_response();
                if let Some(v) = retry.and_then(|v| v.parse().ok()) { r.headers_mut().insert(header::RETRY_AFTER, v); }
                r
            }
            XError::Upstream(e) => { eprintln!("X API error: {e}"); (StatusCode::BAD_GATEWAY, "X API request failed").into_response() }
            XError::Internal(e) => internal(e).into_response(),
        }
    }
}

/// A current access token for the user, refreshing it with X first if it is (nearly) expired.
async fn access_token(state: &AppState, user: &str, force_refresh: bool) -> Result<String, XError> {
    let (Some(cipher), Some(x)) = (&state.auth.cipher, &state.auth.x) else { return Err(XError::NotConnected("X token storage isn't enabled on this server")) };
    let stored = state.repo.get_x_tokens(user).await.map_err(XError::Internal)?.ok_or(XError::NotConnected("no X connection stored"))?;
    if !force_refresh && stored.expires_at > Utc::now() + Duration::seconds(REFRESH_EARLY_SECS) {
        return cipher.open(user, &stored.access_token).ok_or(XError::NotConnected("stored X token can't be decrypted (key changed?)"));
    }
    let refresh = stored.refresh_token.as_deref().and_then(|r| cipher.open(user, r))
        .ok_or(XError::NotConnected("X session expired and can't be refreshed"))?;
    let mut req = state.auth.http.post(&x.token_url).form(&[
        ("grant_type", "refresh_token"), ("refresh_token", refresh.as_str()), ("client_id", x.client_id.as_str()),
    ]);
    if let Some(secret) = &x.client_secret { req = req.basic_auth(&x.client_id, Some(secret)); }
    let resp = req.send().await.map_err(|e| XError::Upstream(e.to_string()))?;
    if resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::UNAUTHORIZED {
        // The refresh token was revoked or already used: the connection is gone.
        state.repo.delete_x_tokens(user).await.map_err(XError::Internal)?;
        return Err(XError::NotConnected("X refused to refresh the session"));
    }
    let fresh: XTokenResponse = resp.error_for_status().map_err(|e| XError::Upstream(e.to_string()))?
        .json().await.map_err(|e| XError::Upstream(e.to_string()))?;
    // X rotates refresh tokens; keep the old one only if it didn't send a new one.
    let fresh = XTokenResponse { refresh_token: fresh.refresh_token.or(Some(refresh)), ..fresh };
    store_tokens(state, user, &fresh).await.map_err(XError::Internal)?;
    Ok(fresh.access_token)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/me/x", get(status).delete(disconnect))
        .route("/api/me/x/timeline", get(timeline))
}

async fn status(State(state): State<AppState>, me: AuthUser) -> Result<Json<Value>, StatusCode> {
    let t = state.repo.get_x_tokens(&me.0).await.map_err(internal)?;
    Ok(Json(json!({
        "connected": t.is_some(),
        "scope": t.as_ref().map(|t| t.scope.clone()),
        "storage_enabled": state.auth.cipher.is_some(),
    })))
}

/// Forget the stored X tokens and ask X to revoke them (best effort).
async fn disconnect(State(state): State<AppState>, me: AuthUser) -> Result<Json<Value>, StatusCode> {
    if let (Some(cipher), Some(x), Some(t)) = (&state.auth.cipher, &state.auth.x, state.repo.get_x_tokens(&me.0).await.map_err(internal)?) {
        if let (Some(url), Some(token)) = (&x.revoke_url, t.refresh_token.as_deref().and_then(|r| cipher.open(&me.0, r))) {
            let mut req = state.auth.http.post(url).form(&[("token", token.as_str()), ("token_type_hint", "refresh_token"), ("client_id", x.client_id.as_str())]);
            if let Some(secret) = &x.client_secret { req = req.basic_auth(&x.client_id, Some(secret)); }
            if let Err(e) = req.send().await.and_then(|r| r.error_for_status()) { eprintln!("X token revoke failed (deleted locally anyway): {e}"); }
        }
    }
    let removed = state.repo.delete_x_tokens(&me.0).await.map_err(internal)?;
    Ok(Json(json!({ "disconnected": removed })))
}

#[derive(Deserialize)]
struct TimelineQuery { #[serde(default = "default_limit")] limit: usize }
fn default_limit() -> usize { 100 }

/// The user's own home timeline from X (newest first), normalized. Only the signed-in user can
/// read their own; nothing here is shared or stored.
async fn timeline(State(state): State<AppState>, me: AuthUser, Query(q): Query<TimelineQuery>) -> Result<Json<Value>, XError> {
    let x = state.auth.x.as_ref().ok_or(XError::NotConnected("X sign-in isn't configured on this server"))?;
    let x_id = me.0.strip_prefix("x-").ok_or(XError::NotConnected("this account didn't sign in with X"))?;
    let limit = q.limit.clamp(1, MAX_TIMELINE);
    let mut token = access_token(&state, &me.0, false).await?;
    let (mut posts, mut next, mut retried) = (Vec::new(), None::<String>, false);
    while posts.len() < limit {
        let mut params = vec![
            ("max_results", (limit - posts.len()).clamp(5, 100).to_string()),
            ("tweet.fields", "author_id,created_at,public_metrics,lang".to_string()),
            ("expansions", "author_id".to_string()),
            ("user.fields", "username,protected,verified".to_string()),
        ];
        if let Some(n) = &next { params.push(("pagination_token", n.clone())); }
        let resp = state.auth.http.get(format!("{}/2/users/{x_id}/timelines/reverse_chronological", x.api_base))
            .bearer_auth(&token).query(&params).send().await.map_err(|e| XError::Upstream(e.to_string()))?;
        match resp.status() {
            StatusCode::UNAUTHORIZED if !retried => {
                retried = true; // revoked or expired early: refresh once and retry
                token = access_token(&state, &me.0, true).await?;
                continue;
            }
            StatusCode::UNAUTHORIZED => return Err(XError::NotConnected("X rejected the stored session")),
            StatusCode::TOO_MANY_REQUESTS => {
                let retry = resp.headers().get(header::RETRY_AFTER).and_then(|v| v.to_str().ok()).map(str::to_string);
                if posts.is_empty() { return Err(XError::RateLimited(retry)); }
                break; // return what we have
            }
            s if !s.is_success() => return Err(XError::Upstream(format!("timeline returned {s}"))),
            _ => {}
        }
        let body: Value = resp.json().await.map_err(|e| XError::Upstream(e.to_string()))?;
        let users: std::collections::HashMap<&str, &Value> = body["includes"]["users"].as_array().into_iter().flatten()
            .filter_map(|u| Some((u["id"].as_str()?, u))).collect();
        let page = body["data"].as_array().cloned().unwrap_or_default();
        for t in &page {
            let author = t["author_id"].as_str().unwrap_or_default();
            let u = users.get(author);
            let m = &t["public_metrics"];
            posts.push(json!({
                "id": t["id"], "text": t["text"], "created_at": t["created_at"], "lang": t["lang"],
                "author_id": author, "author_username": u.and_then(|u| u["username"].as_str()),
                "protected": u.and_then(|u| u["protected"].as_bool()).unwrap_or(false),
                "like_count": m["like_count"].as_i64().unwrap_or(0), "repost_count": m["retweet_count"].as_i64().unwrap_or(0),
                "reply_count": m["reply_count"].as_i64().unwrap_or(0),
                "url": format!("https://x.com/i/status/{}", t["id"].as_str().unwrap_or_default()),
            }));
        }
        next = body["meta"]["next_token"].as_str().map(str::to_string);
        if next.is_none() || page.is_empty() { break; }
    }
    posts.truncate(limit);
    Ok(Json(json!({ "count": posts.len(), "posts": posts })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_binding() {
        let c = TokenCipher::new(&[7u8; 32]).unwrap();
        let sealed = c.seal("x-1", "secret-token");
        assert_eq!(c.open("x-1", &sealed).as_deref(), Some("secret-token"));
        assert_ne!(c.seal("x-1", "secret-token"), sealed, "fresh nonce each time");
        assert!(!String::from_utf8_lossy(&sealed).contains("secret-token"));
        assert_eq!(c.open("x-2", &sealed), None, "bound to the user it was sealed for");
        assert_eq!(TokenCipher::new(&[8u8; 32]).unwrap().open("x-1", &sealed), None, "wrong key");
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert_eq!(c.open("x-1", &tampered), None, "tampering detected");
        assert!(TokenCipher::new(&[0u8; 16]).is_err());
    }
}
