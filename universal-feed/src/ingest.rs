//! Candidate ingestion: dedupe, classify, and attach to public feeds.

use crate::{classify::classify, model::*, store::Store};
use chrono::Utc;
use uuid::Uuid;

const MAX_TEXT_CHARS: usize = 2000;

/// Trimmed/truncated text, or None when the candidate must be rejected.
pub fn prepare_text(c: &Candidate) -> Option<String> {
    let text: String = c.text.trim().chars().take(MAX_TEXT_CHARS).collect();
    (!text.is_empty() && !c.external_id.trim().is_empty()).then_some(text)
}

pub fn clamp_score(c: &Candidate) -> f64 {
    c.score.filter(|s| s.is_finite()).unwrap_or(0.5).clamp(0.0, 1.0)
}

pub fn ingest(store: &Store, candidates: Vec<Candidate>) -> IngestReport {
    let mut report = IngestReport::default();
    // Lock order matches feed::build_home_feed (feeds, then items) to avoid deadlock.
    let mut feeds = store.feeds.write().unwrap();
    let mut items = store.items.write().unwrap();

    for c in candidates {
        let Some(text) = prepare_text(&c) else {
            report.rejected += 1;
            continue;
        };
        let is_dup = items
            .values()
            .any(|i| i.source == c.source && i.external_id.as_deref() == Some(c.external_id.as_str()));
        if is_dup {
            report.duplicates += 1;
            continue;
        }
        let categories = classify(&text, &|cat| feeds.contains_key(cat));
        if categories.is_empty() {
            report.unclassified += 1;
            continue;
        }
        let id = Uuid::new_v4();
        let score = clamp_score(&c);
        items.insert(id, ContentItem {
            id,
            source: c.source,
            external_id: Some(c.external_id.clone()),
            author: c.author,
            text,
            url: c.url,
            categories: categories.clone(),
            created_at: Utc::now(),
            score,
        });
        for cat in &categories {
            if let Some(feed) = feeds.get_mut(cat) { feed.items.push(id); }
        }
        report.ingested.push(IngestedItem { external_id: c.external_id, id, categories });
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{feed::build_home_feed, seed::seed};

    fn cand(ext: &str, text: &str) -> Candidate {
        Candidate { source: Source::X, external_id: ext.into(), author: "a".into(), text: text.into(), url: None, score: Some(0.99) }
    }

    #[test]
    fn ingests_classifies_and_shows_in_home_feed() {
        let store = Store::new();
        seed(&store);
        let r = ingest(&store, vec![cand("1", "Tokio and axum deep dive"), cand("2", "my lunch")]);
        assert_eq!(r.ingested.len(), 1);
        assert_eq!(r.unclassified, 1);
        let home = build_home_feed(&store, "demo").unwrap();
        assert!(home.items.iter().any(|i| i.external_id.as_deref() == Some("1")));
        assert_eq!(home.items[0].external_id.as_deref(), Some("1")); // 0.99 outranks seeds
    }

    #[test]
    fn dedupes_per_source_and_across_batches() {
        let store = Store::new();
        seed(&store);
        let r = ingest(&store, vec![cand("1", "rust tips"), cand("1", "rust tips")]);
        assert_eq!((r.ingested.len(), r.duplicates), (1, 1));
        assert_eq!(ingest(&store, vec![cand("1", "rust tips")]).duplicates, 1);
        let mut tk = cand("1", "rust tips");
        tk.source = Source::TikTok; // same id on a different platform is distinct
        assert_eq!(ingest(&store, vec![tk]).ingested.len(), 1);
    }

    #[test]
    fn rejects_empty_and_clamps_score() {
        let store = Store::new();
        seed(&store);
        let mut hi = cand("9", "rust");
        hi.score = Some(7.0);
        let r = ingest(&store, vec![cand("", "rust"), cand("8", "   "), hi]);
        assert_eq!(r.rejected, 2);
        let items = store.items.read().unwrap();
        let got = items.values().find(|i| i.external_id.as_deref() == Some("9")).unwrap();
        assert_eq!(got.score, 1.0);
    }
}
