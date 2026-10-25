use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source { Local, X, TikTok }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentItem {
    pub id: Uuid,
    pub source: Source,
    /// Id on the originating platform; (source, external_id) is the dedupe key.
    #[serde(default)]
    pub external_id: Option<String>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct Candidate {
    pub source: Source,
    pub external_id: String,
    pub author: String,
    pub text: String,
    #[serde(default)]
    pub url: Option<String>,
    /// Source-side engagement/relevance in [0,1]; clamped. Defaults to 0.5.
    #[serde(default)]
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IngestRequest {
    pub items: Vec<Candidate>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct IngestReport {
    pub ingested: Vec<IngestedItem>,
    pub duplicates: usize,
    pub unclassified: usize,
    pub rejected: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct IngestedItem {
    pub external_id: String,
    pub id: Uuid,
    pub categories: Vec<String>,
}
