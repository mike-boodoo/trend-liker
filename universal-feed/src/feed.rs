use crate::{model::{ContentItem, HomeFeed}, store::Store};

pub fn build_home_feed(store: &Store, user_id: &str) -> Option<HomeFeed> {
    let user = store.get_user(user_id)?;
    let feeds = store.feeds.read().unwrap();
    let items = store.items.read().unwrap();
    let mut result = Vec::<ContentItem>::new();
    let mut seen = std::collections::HashSet::new();
    for feed_id in &user.public_feeds {
        let Some(feed) = feeds.get(feed_id) else { continue };
        for item_id in &feed.items {
            if !seen.insert(*item_id) { continue; }
            if let Some(item) = items.get(item_id) { result.push(item.clone()); }
        }
    }
    result.sort_by(|a,b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    Some(HomeFeed { user_id: user_id.to_string(), feeds: user.public_feeds, items: result })
}
