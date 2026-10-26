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
}

pub fn x_user_id(x_id: &str) -> String { format!("x-{x_id}") }
