//! Storage abstraction: in-memory (demo/tests) or Postgres.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::model::{Candidate, HomeFeed, IngestReport, PublicFeed, PublishingSetting, UserSettings};

pub enum Subscribed {
    Ok(UserSettings),
    NoUser,
    NoFeed,
}

pub enum Ingested {
    Report(IngestReport),
    NoUser,
    NotOptedIn,
}

#[async_trait]
pub trait Repo: Send + Sync {
    async fn list_feeds(&self) -> anyhow::Result<Vec<PublicFeed>>;
    async fn get_user(&self, id: &str) -> anyhow::Result<Option<UserSettings>>;
    async fn subscribe(&self, user: &str, feed: &str) -> anyhow::Result<Subscribed>;
    async fn home_feed(&self, user: &str) -> anyhow::Result<Option<HomeFeed>>;
    /// Enable/disable publishing for a user. Disabling withdraws their contributions.
    async fn set_publishing(&self, user: &str, enabled: bool) -> anyhow::Result<Option<PublishingSetting>>;
    async fn ingest(&self, user: &str, items: Vec<Candidate>) -> anyhow::Result<Ingested>;

    /// Find or create the user linked to an X account (id `x-<x_id>`), refreshing the handle.
    async fn upsert_x_user(&self, x_id: &str, username: &str) -> anyhow::Result<String>;
    /// Create a plain user if missing (dev sign-in only).
    async fn ensure_user(&self, user: &str) -> anyhow::Result<()>;
    async fn create_session(&self, token_hash: &[u8], user: &str, expires_at: DateTime<Utc>) -> anyhow::Result<()>;
    /// The user for an unexpired session.
    async fn session_user(&self, token_hash: &[u8], now: DateTime<Utc>) -> anyhow::Result<Option<String>>;
    async fn delete_session(&self, token_hash: &[u8]) -> anyhow::Result<()>;

    /// Encrypted X tokens for a user (see xapi.rs); replacing any previous ones.
    async fn put_x_tokens(&self, user: &str, tokens: &XTokens) -> anyhow::Result<()>;
    async fn get_x_tokens(&self, user: &str) -> anyhow::Result<Option<XTokens>>;
    async fn delete_x_tokens(&self, user: &str) -> anyhow::Result<bool>;

    async fn register_client(&self, client: &OAuthClient) -> anyhow::Result<()>;
    async fn get_client(&self, client_id: &str) -> anyhow::Result<Option<OAuthClient>>;
    async fn create_refresh_token(&self, token_hash: &[u8], client_id: &str, user: &str, expires_at: DateTime<Utc>) -> anyhow::Result<()>;
    /// Remove and return (client_id, user) for an unexpired refresh token: each one works once.
    async fn take_refresh_token(&self, token_hash: &[u8], now: DateTime<Utc>) -> anyhow::Result<Option<(String, String)>>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct XTokens {
    pub access_token: Vec<u8>,
    pub refresh_token: Option<Vec<u8>>,
    pub expires_at: DateTime<Utc>,
    pub scope: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct OAuthClient {
    pub client_id: String,
    pub client_name: String,
    pub redirect_uris: Vec<String>,
}

pub fn x_user_id(x_id: &str) -> String { format!("x-{x_id}") }
