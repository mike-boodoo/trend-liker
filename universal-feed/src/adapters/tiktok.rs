use anyhow::Result;
use crate::model::ContentItem;

/// Candidate-source placeholder for TikTok. This does not attempt to reproduce
/// TikTok's private consumer For You ranking.
pub async fn fetch_tiktok_candidates(query: &str) -> Result<Vec<ContentItem>> {
    println!("Fetching TikTok candidates for: {query}");
    Ok(Vec::new())
}
