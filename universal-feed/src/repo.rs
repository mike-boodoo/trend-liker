//! Storage abstraction: in-memory (demo/tests) or Postgres.

use async_trait::async_trait;

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
}
