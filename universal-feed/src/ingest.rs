//! Candidate ingestion: dedupe, classify, and attach to public feeds.

use crate::{classify::classify, model::*, repo::Ingested, store::Store};
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

pub fn ingest(store: &Store, user: &str, candidates: Vec<Candidate>) -> Ingested {
    let mut report = IngestReport::default();
    // Lock order (feeds, items, contributors, users) matches Store::set_publishing, so the
    // consent check below cannot race with a withdrawal.
    let mut feeds = store.feeds.write().unwrap();
    let mut items = store.items.write().unwrap();
    let mut contributors = store.contributors.write().unwrap();
    match store.users.read().unwrap().get(user) {
        None => return Ingested::NoUser,
        Some(u) if !u.publish_opt_in => return Ingested::NotOptedIn,
        Some(_) => {}
    }

    for c in candidates {
        let Some(text) = prepare_text(&c) else {
            report.rejected += 1;
            continue;
        };
        let existing = items
            .values()
            .find(|i| i.source == c.source && i.external_id.as_deref() == Some(c.external_id.as_str()))
            .map(|i| i.id);
        if let Some(id) = existing {
            // Already published (possibly by someone else): record this user as a co-contributor.
            if contributors.contains_key(&id) { contributors.entry(id).or_default().insert(user.to_string()); }
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
        contributors.entry(id).or_default().insert(user.to_string());
        for cat in &categories {
            if let Some(feed) = feeds.get_mut(cat) { feed.items.push(id); }
        }
        report.ingested.push(IngestedItem { external_id: c.external_id, id, categories });
    }
    Ingested::Report(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{feed::build_home_feed, repo::Repo, seed::seed};

    fn cand(ext: &str, text: &str) -> Candidate {
        Candidate { source: Source::X, external_id: ext.into(), author: "a".into(), text: text.into(), url: None, score: Some(0.99) }
    }

    fn report(i: Ingested) -> IngestReport {
        match i { Ingested::Report(r) => r, _ => panic!("expected a report") }
    }

    /// Seeded store with the demo user opted in, plus a second opted-in user "bob".
    fn opted_in_store() -> Store {
        let store = Store::new();
        seed(&store);
        store.users.write().unwrap().get_mut("demo").unwrap().publish_opt_in = true;
        store.insert_user(UserSettings { user_id: "bob".into(), public_feeds: vec![], publish_opt_in: true, username: None });
        store
    }

    #[test]
    fn ingests_classifies_and_shows_in_home_feed() {
        let store = opted_in_store();
        let r = report(ingest(&store, "demo", vec![cand("1", "Tokio and axum deep dive"), cand("2", "my lunch")]));
        assert_eq!(r.ingested.len(), 1);
        assert_eq!(r.unclassified, 1);
        let home = build_home_feed(&store, "demo").unwrap();
        assert_eq!(home.items[0].external_id.as_deref(), Some("1")); // 0.99 outranks seeds
    }

    #[test]
    fn dedupes_per_source_and_across_batches() {
        let store = opted_in_store();
        let r = report(ingest(&store, "demo", vec![cand("1", "rust tips"), cand("1", "rust tips")]));
        assert_eq!((r.ingested.len(), r.duplicates), (1, 1));
        assert_eq!(report(ingest(&store, "demo", vec![cand("1", "rust tips")])).duplicates, 1);
        let mut tk = cand("1", "rust tips");
        tk.source = Source::TikTok; // same id on a different platform is distinct
        assert_eq!(report(ingest(&store, "demo", vec![tk])).ingested.len(), 1);
    }

    #[test]
    fn rejects_empty_and_clamps_score() {
        let store = opted_in_store();
        let mut hi = cand("9", "rust");
        hi.score = Some(7.0);
        let r = report(ingest(&store, "demo", vec![cand("", "rust"), cand("8", "   "), hi]));
        assert_eq!(r.rejected, 2);
        let items = store.items.read().unwrap();
        let got = items.values().find(|i| i.external_id.as_deref() == Some("9")).unwrap();
        assert_eq!(got.score, 1.0);
    }

    #[test]
    fn requires_opt_in_and_known_user() {
        let store = Store::new();
        seed(&store); // demo is NOT opted in
        assert!(matches!(ingest(&store, "demo", vec![cand("1", "rust")]), Ingested::NotOptedIn));
        assert!(matches!(ingest(&store, "ghost", vec![cand("1", "rust")]), Ingested::NoUser));
        assert_eq!(store.items.read().unwrap().len(), 6, "nothing stored without consent");
    }

    #[tokio::test]
    async fn opting_out_withdraws_only_unshared_contributions() {
        let store = opted_in_store();
        report(ingest(&store, "demo", vec![cand("solo", "rust solo"), cand("shared", "rust shared")]));
        report(ingest(&store, "bob", vec![cand("shared", "rust shared")])); // bob co-contributes
        let out = store.set_publishing("demo", false).await.unwrap().unwrap();
        assert_eq!((out.enabled, out.withdrawn), (false, 1));
        let items = store.items.read().unwrap();
        assert!(!items.values().any(|i| i.external_id.as_deref() == Some("solo")));
        assert!(items.values().any(|i| i.external_id.as_deref() == Some("shared")));
        assert_eq!(items.len(), 7, "seed items untouched");
        let feeds = store.feeds.read().unwrap();
        assert!(feeds.values().all(|f| f.items.iter().all(|id| items.contains_key(id))), "no dangling feed refs");
        drop((items, feeds));
        assert!(matches!(ingest(&store, "demo", vec![cand("again", "rust")]), Ingested::NotOptedIn));
        // Bob leaving removes the last contribution.
        assert_eq!(store.set_publishing("bob", false).await.unwrap().unwrap().withdrawn, 1);
        assert!(store.set_publishing("ghost", true).await.unwrap().is_none());
    }
}
