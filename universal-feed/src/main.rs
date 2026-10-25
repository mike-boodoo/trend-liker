mod model;
mod store;
mod seed;
mod feed;
mod classify;
mod ingest;
mod adapters;

use axum::{extract::{Path, State}, http::StatusCode, response::Html, routing::{get, post}, Json, Router};
use serde::Deserialize;
use store::Store;

#[derive(Clone)] struct AppState { store: Store }
#[derive(Debug, Deserialize)] struct SubscriptionRequest { feed: String }

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let store = Store::new();
    seed::seed(&store);
    let state = AppState { store };
    let app = Router::new()
        .route("/", get(index))
        .route("/api/feeds", get(list_feeds))
        .route("/api/ingest", post(ingest_candidates))
        .route("/api/users/{user}/feed", get(user_feed))
        .route("/api/users/{user}/subscriptions", get(get_subscriptions))
        .route("/api/users/{user}/subscriptions", post(subscribe))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.expect("failed to bind localhost:3000");
    println!("Universal Feed Server\nhttp://localhost:3000");
    axum::serve(listener, app).await.expect("server failed");
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
<div class="panel"><h3>Public feed subscriptions</h3><div id="subscriptions"></div></div><div id="feed"></div>
<script>
const USER="demo";
async function api(path,options={}){const r=await fetch(path,options);if(!r.ok)throw new Error(await r.text());return r.json();}
async function loadSubscriptions(){const [settings,feeds]=await Promise.all([api(`/api/users/${USER}/subscriptions`),api(`/api/feeds`)]);const selected=new Set(settings.public_feeds);document.getElementById("subscriptions").innerHTML=feeds.map(feed=>`<button class="${selected.has(feed.id)?"active":""}" onclick="toggleFeed('${feed.id}',${selected.has(feed.id)})">${feed.name}</button>`).join("");}
async function toggleFeed(feed,alreadySelected){if(alreadySelected){alert("The demo supports adding subscriptions. Restart the server to reset.");return;}await api(`/api/users/${USER}/subscriptions`,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify({feed})});await loadSubscriptions();await loadFeed();}
async function loadFeed(){const data=await api(`/api/users/${USER}/feed`);document.getElementById("feed").innerHTML=data.items.map(item=>`<article class="feed"><div class="meta">${item.source} · ${escapeHtml(item.author)} · score ${item.score.toFixed(2)}</div><h3>${escapeHtml(item.text)}</h3><div>${item.categories.map(c=>`<span class="category">${escapeHtml(c)}</span>`).join("")}</div></article>`).join("");}
function escapeHtml(value){return value.replace(/[&<>"']/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;","\"":"&quot;","'":"&#039;"}[c]));}
loadSubscriptions();loadFeed();
</script></body></html>"##)
}

async fn list_feeds(State(state): State<AppState>) -> Json<Vec<model::PublicFeed>> {
    let feeds = state.store.feeds.read().unwrap();
    Json(feeds.values().cloned().collect())
}
async fn user_feed(State(state): State<AppState>, Path(user): Path<String>) -> Result<Json<model::HomeFeed>, StatusCode> {
    feed::build_home_feed(&state.store, &user).map(Json).ok_or(StatusCode::NOT_FOUND)
}
async fn get_subscriptions(State(state): State<AppState>, Path(user): Path<String>) -> Result<Json<model::UserSettings>, StatusCode> {
    state.store.get_user(&user).map(Json).ok_or(StatusCode::NOT_FOUND)
}
async fn subscribe(State(state): State<AppState>, Path(user): Path<String>, Json(request): Json<SubscriptionRequest>) -> Result<Json<model::UserSettings>, StatusCode> {
    if !state.store.feeds.read().unwrap().contains_key(&request.feed) { return Err(StatusCode::BAD_REQUEST); }
    let mut users = state.store.users.write().unwrap();
    let settings = users.get_mut(&user).ok_or(StatusCode::NOT_FOUND)?;
    if !settings.public_feeds.contains(&request.feed) { settings.public_feeds.push(request.feed); }
    Ok(Json(settings.clone()))
}

const MAX_INGEST_BATCH: usize = 500;

async fn ingest_candidates(State(state): State<AppState>, Json(request): Json<model::IngestRequest>) -> Result<Json<model::IngestReport>, StatusCode> {
    if request.items.len() > MAX_INGEST_BATCH { return Err(StatusCode::PAYLOAD_TOO_LARGE); }
    Ok(Json(ingest::ingest(&state.store, request.items)))
}
