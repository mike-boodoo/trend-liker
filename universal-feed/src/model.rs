use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Source { Local, X, TikTok }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentItem {
    pub id: Uuid,
    pub source: Source,
    pub author: String,
    pub text: String,
    pub url: Option<String>,
    pub categories: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicFeed {
    pub id: String,
    pub name: String,
    pub description: String,
    pub categories: Vec<String>,
    pub items: Vec<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserSettings {
    pub user_id: String,
    pub public_feeds: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeFeed {
    pub user_id: String,
    pub feeds: Vec<String>,
    pub items: Vec<ContentItem>,
}
