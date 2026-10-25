use chrono::Utc;
use uuid::Uuid;
use crate::{model::{ContentItem, PublicFeed, Source, UserSettings}, store::Store};

pub fn seed(store: &Store) {
    let posts = vec![
        ("Rust 1.90 introduces new improvements", "Rust", vec!["rust", "programming"], 0.94),
        ("New developments in AI inference infrastructure", "AI Research", vec!["ai", "cloud", "infrastructure"], 0.98),
        ("Kubernetes scheduling patterns for distributed workloads", "Cloud Engineering", vec!["cloud", "kubernetes", "infrastructure"], 0.91),
        ("New open source agent framework released", "AI Developer", vec!["ai", "agents", "programming"], 0.96),
        ("Efficient vector search using HNSW", "Systems Research", vec!["ai", "search", "programming"], 0.89),
        ("Building high-throughput Rust services", "Backend Engineering", vec!["rust", "cloud", "programming"], 0.93),
    ];
    let mut categorized = std::collections::HashMap::<String, Vec<Uuid>>::new();
    for (text, author, categories, score) in posts {
        let id = Uuid::new_v4();
        store.insert_item(ContentItem {
            id, source: Source::Local, external_id: None, author: author.to_string(), text: text.to_string(), url: None,
            categories: categories.iter().map(|x| x.to_string()).collect(), created_at: Utc::now(), score,
        });
        for category in categories { categorized.entry(category.to_string()).or_default().push(id); }
    }
    for (category, item_ids) in categorized {
        store.insert_feed(PublicFeed {
            id: category.clone(), name: category.clone(), description: format!("Public {} feed", category),
            categories: vec![category], items: item_ids,
        });
    }
    store.insert_user(UserSettings { user_id: "demo".into(), public_feeds: vec!["ai".into(), "rust".into(), "cloud".into()] });
}
