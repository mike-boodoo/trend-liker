//! HTTP routes for lobbies, including a Server-Sent Events stream of re-rank events.

use std::convert::Infallible;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{delete, get, post, put},
    Json, Router,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use tokio_stream::{wrappers::BroadcastStream, Stream, StreamExt};
use uuid::Uuid;

use crate::lobby::{ContributeReport, Kind, Lobby, LobbyError, LobbyHandle, LobbyPolicy, LobbyProfile, Ranked};
use crate::model::Candidate;
use crate::{internal, AppState, MAX_INGEST_BATCH};

const MAX_FEED_LIMIT: usize = 200;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/lobbies", get(list).post(create))
        .route("/api/lobbies/{id}", get(profile))
        .route("/api/lobbies/{id}/policy", put(set_policy))
        .route("/api/lobbies/{id}/members", post(join))
        .route("/api/lobbies/{id}/members/{user}", delete(leave))
        .route("/api/lobbies/{id}/contribute", post(contribute))
        .route("/api/lobbies/{id}/interactions", post(interact))
        .route("/api/lobbies/{id}/feed", get(feed))
        .route("/api/lobbies/{id}/events", get(events))
}

fn status(e: LobbyError) -> StatusCode {
    match e {
        LobbyError::NotMember => StatusCode::FORBIDDEN,
        LobbyError::NoItem => StatusCode::NOT_FOUND,
        LobbyError::Full | LobbyError::OwnerCannotLeave => StatusCode::CONFLICT,
        LobbyError::InvalidPolicy(_) => StatusCode::UNPROCESSABLE_ENTITY,
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn handle(state: &AppState, id: &str) -> Result<std::sync::Arc<LobbyHandle>, StatusCode> {
    state.lobbies.get(id).ok_or(StatusCode::NOT_FOUND)
}

async fn require_user(state: &AppState, user: &str) -> Result<crate::model::UserSettings, StatusCode> {
    state.repo.get_user(user).await.map_err(internal)?.ok_or(StatusCode::NOT_FOUND)
}

async fn list(State(state): State<AppState>) -> Json<Vec<LobbyProfile>> {
    Json(state.lobbies.all().iter().map(|h| h.lobby.lock().unwrap().profile()).collect())
}

#[derive(Deserialize)]
struct CreateLobby {
    id: String,
    name: String,
    owner: String,
    #[serde(default)]
    policy: LobbyPolicy,
}

async fn create(State(state): State<AppState>, Json(r): Json<CreateLobby>) -> Result<(StatusCode, Json<LobbyProfile>), StatusCode> {
    if !valid_id(&r.id) { return Err(StatusCode::BAD_REQUEST); }
    require_user(&state, &r.owner).await?;
    let lobby = Lobby::new(&r.id, &r.name, &r.owner, r.policy).map_err(status)?;
    let h = state.lobbies.create(lobby).ok_or(StatusCode::CONFLICT)?;
    let profile = h.lobby.lock().unwrap().profile();
    Ok((StatusCode::CREATED, Json(profile)))
}

async fn profile(State(state): State<AppState>, Path(id): Path<String>) -> Result<Json<LobbyProfile>, StatusCode> {
    Ok(Json(handle(&state, &id)?.lobby.lock().unwrap().profile()))
}

#[derive(Deserialize)]
struct SetPolicy { user_id: String, policy: LobbyPolicy }

/// Owner only.
async fn set_policy(State(state): State<AppState>, Path(id): Path<String>, Json(r): Json<SetPolicy>) -> Result<Json<LobbyProfile>, StatusCode> {
    let h = handle(&state, &id)?;
    let (profile, version) = {
        let mut l = h.lobby.lock().unwrap();
        if l.owner != r.user_id { return Err(StatusCode::FORBIDDEN); }
        l.set_policy(r.policy, Utc::now()).map_err(status)?;
        (l.profile(), l.version())
    };
    h.publish(version, "policy", None);
    Ok(Json(profile))
}

#[derive(Deserialize)]
struct UserBody { user_id: String }

async fn join(State(state): State<AppState>, Path(id): Path<String>, Json(r): Json<UserBody>) -> Result<Json<LobbyProfile>, StatusCode> {
    let h = handle(&state, &id)?;
    require_user(&state, &r.user_id).await?;
    let (joined, profile) = {
        let mut l = h.lobby.lock().unwrap();
        (l.join(&r.user_id).map_err(status)?, l.profile())
    };
    if joined { h.publish(profile.version, "member", None); }
    Ok(Json(profile))
}

async fn leave(State(state): State<AppState>, Path((id, user)): Path<(String, String)>) -> Result<Json<serde_json::Value>, StatusCode> {
    let h = handle(&state, &id)?;
    let (withdrawn, version) = {
        let mut l = h.lobby.lock().unwrap();
        (l.leave(&user).map_err(status)?, l.version())
    };
    h.publish(version, "member", None);
    Ok(Json(json!({ "withdrawn": withdrawn })))
}

#[derive(Deserialize)]
struct Contribute { user_id: String, items: Vec<Candidate> }

/// Share posts from the member's own feed. Requires the publishing opt-in (403 otherwise).
async fn contribute(State(state): State<AppState>, Path(id): Path<String>, Json(r): Json<Contribute>) -> Result<Json<ContributeReport>, StatusCode> {
    if r.items.len() > MAX_INGEST_BATCH { return Err(StatusCode::PAYLOAD_TOO_LARGE); }
    let h = handle(&state, &id)?;
    if !require_user(&state, &r.user_id).await?.publish_opt_in { return Err(StatusCode::FORBIDDEN); }
    let (report, version) = {
        let mut l = h.lobby.lock().unwrap();
        (l.contribute(&r.user_id, r.items, Utc::now()).map_err(status)?, l.version())
    };
    h.publish(version, "contribution", None);
    // Consent may have been withdrawn between the check and the insert; if so, undo.
    if !require_user(&state, &r.user_id).await?.publish_opt_in {
        state.lobbies.withdraw_everywhere(&r.user_id);
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(Json(report))
}

#[derive(Deserialize)]
struct Interaction {
    user_id: String,
    item_id: Uuid,
    kind: Kind,
    #[serde(default = "yes")]
    active: bool,
}
fn yes() -> bool { true }

async fn interact(State(state): State<AppState>, Path(id): Path<String>, Json(r): Json<Interaction>) -> Result<Json<serde_json::Value>, StatusCode> {
    let h = handle(&state, &id)?;
    let version = {
        let mut l = h.lobby.lock().unwrap();
        l.interact(&r.user_id, r.item_id, r.kind, r.active, Utc::now()).map_err(status)?;
        l.version()
    };
    h.publish(version, "interaction", Some(r.item_id));
    Ok(Json(json!({ "version": version })))
}

#[derive(Deserialize)]
struct FeedQuery { user: String, #[serde(default = "default_limit")] limit: usize }
fn default_limit() -> usize { 50 }

async fn feed(State(state): State<AppState>, Path(id): Path<String>, Query(q): Query<FeedQuery>) -> Result<Json<serde_json::Value>, StatusCode> {
    let h = handle(&state, &id)?;
    let l = h.lobby.lock().unwrap();
    if !l.is_member(&q.user) { return Err(StatusCode::FORBIDDEN); }
    let items: Vec<Ranked> = l.rank(&q.user, Utc::now(), q.limit.clamp(1, MAX_FEED_LIMIT));
    Ok(Json(json!({ "lobby": l.id, "user": q.user, "version": l.version(), "items": items })))
}

#[derive(Deserialize)]
struct EventsQuery { user: String }

/// Members only. Sends `hello` with the current version, then one event per change;
/// clients refetch their feed when the version moves. `resync` means events were dropped.
async fn events(State(state): State<AppState>, Path(id): Path<String>, Query(q): Query<EventsQuery>)
    -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, StatusCode>
{
    let h = handle(&state, &id)?;
    let (rx, version) = {
        let l = h.lobby.lock().unwrap();
        if !l.is_member(&q.user) { return Err(StatusCode::FORBIDDEN); }
        (h.events.subscribe(), l.version())
    };
    let hello = Event::default().event("hello").json_data(json!({ "lobby": id, "version": version })).unwrap();
    let updates = BroadcastStream::new(rx).map(|msg| Ok(match msg {
        Ok(ev) => Event::default().event(ev.kind.clone()).json_data(&ev).unwrap(),
        Err(_) => Event::default().event("resync").data("{}"),
    }));
    let stream = tokio_stream::once(Ok(hello)).chain(updates);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}
