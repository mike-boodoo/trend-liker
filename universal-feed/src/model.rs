use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source { Local, X, TikTok }

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self { Source::Local => "Local", Source::X => "X", Source::TikTok => "TikTok" }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s { "Local" => Some(Source::Local), "X" => Some(Source::X), "TikTok" => Some(Source::TikTok), _ => None }
    }
}

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
    /// Whether this user's content may be published into shared feeds. Off by default.
    #[serde(default)]
    pub publish_opt_in: bool,
    /// X handle, for users who signed in with X.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
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
    /// Optional; must match the signed-in user if given.
    #[serde(default)]
    pub user_id: Option<String>,
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

#[derive(Debug, Clone, Deserialize)]
pub struct PublishingRequest {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PublishingSetting {
    pub user_id: String,
    pub enabled: bool,
    /// Items removed from shared feeds because this user withdrew consent.
    pub withdrawn: usize,
}
