//! Keyword classifier: maps free text to public-feed categories.

const KEYWORDS: &[(&str, &[&str])] = &[
    ("ai", &["ai", "llm", "llms", "gpt", "inference", "transformer", "transformers", "neural", "ml"]),
    ("agents", &["agent", "agents", "agentic", "mcp"]),
    ("rust", &["rust", "cargo", "rustc", "tokio", "axum"]),
    ("cloud", &["cloud", "aws", "gcp", "azure", "serverless"]),
    ("kubernetes", &["kubernetes", "k8s", "kubectl", "helm"]),
    ("infrastructure", &["infrastructure", "infra", "terraform", "datacenter", "datacenters"]),
    ("search", &["search", "vector", "embeddings", "hnsw", "retrieval"]),
    ("programming", &["programming", "compiler", "api", "framework", "library", "opensource", "refactor"]),
];

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Returns matching categories (restricted to `allowed`), in KEYWORDS order.
pub fn classify(text: &str, allowed: &dyn Fn(&str) -> bool) -> Vec<String> {
    let toks = tokens(text);
    KEYWORDS
        .iter()
        .filter(|(cat, kws)| allowed(cat) && kws.iter().any(|k| toks.iter().any(|t| t == k)))
        .map(|(cat, _)| cat.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn any(_: &str) -> bool { true }

    #[test]
    fn matches_whole_words_only() {
        assert_eq!(classify("Rust and Cargo are great", &any), vec!["rust"]);
        assert!(classify("Trust the process, said the rustic maid", &any).is_empty());
        assert!(classify("I said hi", &any).is_empty()); // "ai" not inside "said"
    }

    #[test]
    fn multiple_categories_and_hashtags() {
        let c = classify("#AI #agents on #k8s", &any);
        assert_eq!(c, vec!["ai", "agents", "kubernetes"]);
    }

    #[test]
    fn respects_allowed_feeds() {
        let only_ai = |c: &str| c == "ai";
        assert_eq!(classify("AI agents", &only_ai), vec!["ai"]);
    }
}
