mod model;
mod store;
mod seed;
mod feed;
mod classify;
mod ingest;
mod pg;
mod repo;
mod lobby;
mod lobby_api;
mod auth;
mod pages;
mod oauth;
mod xapi;
mod adapters;

use axum::{extract::{Path, State}, http::StatusCode, response::Html, routing::{get, post, put}, Json, Router};
use serde::Deserialize;
use std::sync::Arc;
use auth::AuthUser;
use repo::{Ingested, Repo, Subscribed};
use store::Store;

#[derive(Clone)] pub struct AppState { pub repo: Arc<dyn Repo>, pub lobbies: Arc<lobby::Lobbies>, pub auth: Arc<auth::Auth>, pub oauth: Arc<oauth::OAuth> }
#[derive(Debug, Deserialize)] struct SubscriptionRequest { feed: String }

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let repo: Arc<dyn Repo> = match std::env::var("DATABASE_URL").ok().filter(|u| !u.is_empty()) {
        Some(url) => {
            let pg = pg::PgRepo::connect(&url, None).await.expect("failed to connect to Postgres");
            if std::env::var("SEED_DEMO").map(|v| v != "0").unwrap_or(true) {
                if pg.seed_if_empty().await.expect("seeding failed") { println!("Seeded demo data into Postgres"); }
            }
            println!("Storage: Postgres");
            Arc::new(pg)
        }
        None => {
            let store = Store::new();
            seed::seed(&store);
            println!("Storage: in-memory (set DATABASE_URL for Postgres)");
            Arc::new(store)
        }
    };
    let auth = auth::Auth::from_env();
    match (&auth.x, auth.dev_login) {
        (Some(_), false) => println!("Sign-in: X"),
        (_, true) => eprintln!("WARNING: AUTH_DEV_LOGIN=1 lets anyone sign in as anyone. Never enable it outside local development."),
        (None, false) => eprintln!("WARNING: no sign-in configured (set X_CLIENT_ID); every API that needs a user will return 401."),
    }
    if auth.x.is_some() && auth.cipher.is_none() {
        eprintln!("NOTE: TOKEN_ENCRYPTION_KEY is not set, so X tokens aren't stored and timeline features are off.");
    }
    let oauth = oauth::OAuth::new(oauth::OAuthConfig::from_env());
    println!("Connector sign-in (OAuth issuer): {}; MCP resources: {}", oauth.config.issuer, oauth.config.resources.join(", "));
    let state = AppState { repo, lobbies: Arc::new(lobby::Lobbies::default()), auth: Arc::new(auth), oauth: Arc::new(oauth) };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.expect("failed to bind localhost:3000");
    println!("Universal Feed Server\nhttp://localhost:3000");
    axum::serve(listener, app(state)).await.expect("server failed");
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/feeds", get(list_feeds))
        .route("/api/classify", post(classify_texts))
        .route("/dev", get(pages::dev))
        .route("/lobbies/{id}", get(pages::lobby))
        .route("/api/ingest", post(ingest_candidates))
        .route("/api/users/{user}/publishing", put(set_publishing))
        .route("/api/users/{user}/feed", get(user_feed))
        .route("/api/users/{user}/subscriptions", get(get_subscriptions))
        .route("/api/users/{user}/subscriptions", post(subscribe))
        .merge(lobby_api::routes())
        .merge(auth::routes())
        .merge(oauth::routes())
        .merge(xapi::routes())
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(r##"<!DOCTYPE html>
<html><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Universal Feed — For Us</title>
<style>
body{font-family:system-ui,sans-serif;max-width:900px;margin:auto;padding:30px;background:#f5f5f5;color:#111}
h1{margin-bottom:5px}.subtitle{color:#666;margin-bottom:25px}.panel,.feed{background:white;padding:20px;border-radius:12px;margin-bottom:15px}
.category{display:inline-block;background:#eee;padding:5px 9px;border-radius:7px;margin:3px;font-size:13px}button{border:0;padding:8px 12px;border-radius:7px;cursor:pointer;margin:3px}button.active{font-weight:700}.meta{color:#777;font-size:13px}
</style></head><body>
<h1>For Us</h1><div class="subtitle">Shared public feeds selected through your private <b>For Me</b> settings.</div>
<div class="panel" id="account"></div>
<div class="panel"><h3>Public feed subscriptions</h3><div id="subscriptions"></div></div><div id="feed"></div>
<script>
let USER=null;
async function api(path,options={}){const r=await fetch(path,options);if(!r.ok)throw new Error(await r.text());return r.json();}
async function loadSubscriptions(){const [settings,feeds]=await Promise.all([api(`/api/users/${USER}/subscriptions`),api(`/api/feeds`)]);const selected=new Set(settings.public_feeds);document.getElementById("subscriptions").innerHTML=feeds.map(feed=>`<button class="${selected.has(feed.id)?"active":""}" onclick="toggleFeed('${feed.id}',${selected.has(feed.id)})">${feed.name}</button>`).join("");}
async function toggleFeed(feed,alreadySelected){if(alreadySelected){alert("The demo supports adding subscriptions. Restart the server to reset.");return;}await api(`/api/users/${USER}/subscriptions`,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify({feed})});await loadSubscriptions();await loadFeed();}
async function loadFeed(){const data=await api(`/api/users/${USER}/feed`);document.getElementById("feed").innerHTML=data.items.map(item=>`<article class="feed"><div class="meta">${item.source} · ${escapeHtml(item.author)} · score ${item.score.toFixed(2)}</div><h3>${escapeHtml(item.text)}</h3><div>${item.categories.map(c=>`<span class="category">${escapeHtml(c)}</span>`).join("")}</div></article>`).join("");}
function escapeHtml(value){return value.replace(/[&<>"']/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;","'":"&#039;"}[c]));}
async function start(){const r=await fetch("/api/me");const account=document.getElementById("account");
if(r.status===401){account.innerHTML=`<a href="/auth/x/login">Sign in with X</a>`;return;}
const me=await r.json();USER=me.user_id;
account.innerHTML=`Signed in as <b>${escapeHtml(me.username?"@"+me.username:me.user_id)}</b> <button onclick="logout()">Sign out</button>`;
loadSubscriptions();loadFeed();}
async function logout(){await fetch("/auth/logout",{method:"POST"});location.reload();}
start();
</script></body></html>"##)
}

pub fn internal<E: std::fmt::Display>(e: E) -> StatusCode {
    eprintln!("internal error: {e:#}");
    StatusCode::INTERNAL_SERVER_ERROR
}

async fn list_feeds(State(state): State<AppState>) -> Result<Json<Vec<model::PublicFeed>>, StatusCode> {
    state.repo.list_feeds().await.map(Json).map_err(internal)
}
async fn user_feed(State(state): State<AppState>, me: AuthUser, Path(user): Path<String>) -> Result<Json<model::HomeFeed>, StatusCode> {
    let user = me.claim(Some(&user))?;
    state.repo.home_feed(user).await.map_err(internal)?.map(Json).ok_or(StatusCode::NOT_FOUND)
}
async fn get_subscriptions(State(state): State<AppState>, me: AuthUser, Path(user): Path<String>) -> Result<Json<model::UserSettings>, StatusCode> {
    let user = me.claim(Some(&user))?;
    state.repo.get_user(user).await.map_err(internal)?.map(Json).ok_or(StatusCode::NOT_FOUND)
}
async fn subscribe(State(state): State<AppState>, me: AuthUser, Path(user): Path<String>, Json(request): Json<SubscriptionRequest>) -> Result<Json<model::UserSettings>, StatusCode> {
    let user = me.claim(Some(&user))?;
    match state.repo.subscribe(user, &request.feed).await.map_err(internal)? {
        Subscribed::Ok(settings) => Ok(Json(settings)),
        Subscribed::NoUser => Err(StatusCode::NOT_FOUND),
        Subscribed::NoFeed => Err(StatusCode::BAD_REQUEST),
    }
}

pub const MAX_INGEST_BATCH: usize = 500;

#[derive(Deserialize)]
struct ClassifyRequest { texts: Vec<String> }

/// Labels the server would give each text (stateless, public). Lets clients split a feed
/// into the same categories the server uses.
async fn classify_texts(Json(r): Json<ClassifyRequest>) -> Result<Json<serde_json::Value>, StatusCode> {
    if r.texts.len() > MAX_INGEST_BATCH { return Err(StatusCode::PAYLOAD_TOO_LARGE); }
    let results: Vec<_> = r.texts.iter().map(|t| {
        let t: String = t.chars().take(2000).collect();
        serde_json::json!({ "labels": classify::classify(&t, &|_| true), "finance": classify::is_finance(&t) })
    }).collect();
    Ok(Json(serde_json::json!({ "results": results })))
}

/// 404 unknown user, 403 user has not opted in to publishing.
async fn ingest_candidates(State(state): State<AppState>, me: AuthUser, Json(request): Json<model::IngestRequest>) -> Result<Json<model::IngestReport>, StatusCode> {
    if request.items.len() > MAX_INGEST_BATCH { return Err(StatusCode::PAYLOAD_TOO_LARGE); }
    let user = me.claim(request.user_id.as_deref())?;
    match state.repo.ingest(user, request.items).await.map_err(internal)? {
        Ingested::Report(r) => Ok(Json(r)),
        Ingested::NoUser => Err(StatusCode::NOT_FOUND),
        Ingested::NotOptedIn => Err(StatusCode::FORBIDDEN),
    }
}

/// Turning publishing off also withdraws the user's contributions from shared feeds.
async fn set_publishing(State(state): State<AppState>, me: AuthUser, Path(user): Path<String>, Json(request): Json<model::PublishingRequest>) -> Result<Json<model::PublishingSetting>, StatusCode> {
    let user = me.claim(Some(&user))?.to_string();
    let mut setting = state.repo.set_publishing(&user, request.enabled).await.map_err(internal)?.ok_or(StatusCode::NOT_FOUND)?;
    if !request.enabled { setting.withdrawn += state.lobbies.withdraw_everywhere(&user); }
    Ok(Json(setting))
}
