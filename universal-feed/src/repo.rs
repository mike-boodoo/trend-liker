//! Storage abstraction: in-memory (demo/tests) or Postgres.

use async_trait::async_trait;

use crate::model::{Candidate, HomeFeed, IngestReport, PublicFeed, UserSettings};

pub enum Subscribed {
    Ok(UserSettings),
    NoUser,
    NoFeed,
}

#[async_trait]
pub trait Repo: Send + Sync {
    async fn list_feeds(&self) -> anyhow::Result<Vec<PublicFeed>>;
    async fn get_user(&self, id: &str) -> anyhow::Result<Option<UserSettings>>;
    async fn subscribe(&self, user: &str, feed: &str) -> anyhow::Result<Subscribed>;
    async fn home_feed(&self, user: &str) -> anyhow::Result<Option<HomeFeed>>;
    async fn ingest(&self, items: Vec<Candidate>) -> anyhow::Result<IngestReport>;
}
