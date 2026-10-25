use chrono::Utc;
use uuid::Uuid;
use crate::{model::{ContentItem, PublicFeed, Source, UserSettings}, store::Store};

pub struct DemoData {
    pub items: Vec<ContentItem>,
    pub feeds: Vec<PublicFeed>,
    pub users: Vec<UserSettings>,
}

pub fn demo_data() -> DemoData {
    let posts = vec![
        ("Rust 1.90 introduces new improvements", "Rust", vec!["rust", "programming"], 0.94),
        ("New developments in AI inference infrastructure", "AI Research", vec!["ai", "cloud", "infrastructure"], 0.98),
        ("Kubernetes scheduling patterns for distributed workloads", "Cloud Engineering", vec!["cloud", "kubernetes", "infrastructure"], 0.91),
        ("New open source agent framework released", "AI Developer", vec!["ai", "agents", "programming"], 0.96),
        ("Efficient vector search using HNSW", "Systems Research", vec!["ai", "search", "programming"], 0.89),
        ("Building high-throughput Rust services", "Backend Engineering", vec!["rust", "cloud", "programming"], 0.93),
    ];
    let mut items = Vec::new();
    let mut categorized = std::collections::BTreeMap::<String, Vec<Uuid>>::new();
    for (text, author, categories, score) in posts {
        let id = Uuid::new_v4();
        items.push(ContentItem {
            id, source: Source::Local, external_id: None, author: author.to_string(), text: text.to_string(), url: None,
            categories: categories.iter().map(|x| x.to_string()).collect(), created_at: Utc::now(), score,
        });
        for category in categories { categorized.entry(category.to_string()).or_default().push(id); }
    }
    let feeds = categorized.into_iter().map(|(category, item_ids)| PublicFeed {
        id: category.clone(), name: category.clone(), description: format!("Public {} feed", category),
        categories: vec![category], items: item_ids,
    }).collect();
    let users = vec![UserSettings { user_id: "demo".into(), public_feeds: vec!["ai".into(), "rust".into(), "cloud".into()] }];
    DemoData { items, feeds, users }
}

pub fn seed(store: &Store) {
    let d = demo_data();
    for i in d.items { store.insert_item(i); }
    for f in d.feeds { store.insert_feed(f); }
    for u in d.users { store.insert_user(u); }
}
