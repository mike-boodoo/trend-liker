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
    // Also matched by cashtags ($AAPL); see `has_cashtag`.
    ("finance", &["stock", "stocks", "earnings", "shares", "dividend", "dividends", "ipo", "bullish",
                  "bearish", "nasdaq", "portfolio", "trading", "ticker", "etf", "options"]),
];

fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// `$` followed by 1-5 letters, e.g. "$AAPL" or "$tsla," (not "$5").
fn has_cashtag(text: &str) -> bool {
    text.split_whitespace().any(|w| {
        let w = w.trim_matches(|c: char| !c.is_alphanumeric() && c != '$');
        w.len() >= 2 && w.len() <= 6 && w.starts_with('$') && w[1..].chars().all(|c| c.is_ascii_alphabetic())
    })
}

/// Returns matching categories (restricted to `allowed`), in KEYWORDS order.
pub fn classify(text: &str, allowed: &dyn Fn(&str) -> bool) -> Vec<String> {
    let toks = tokens(text);
    KEYWORDS
        .iter()
        .filter(|(cat, kws)| {
            allowed(cat)
                && (kws.iter().any(|k| toks.iter().any(|t| t == k)) || (*cat == "finance" && has_cashtag(text)))
        })
        .map(|(cat, _)| cat.to_string())
        .collect()
}

/// Market/stock content: gets no cross-user amplification by default (pump-and-dump risk).
pub fn is_finance(text: &str) -> bool {
    !classify(text, &|c| c == "finance").is_empty()
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
    fn detects_finance_and_cashtags() {
        assert!(is_finance("Loading up on $NVDA before earnings"));
        assert!(is_finance("$tsla, to the moon"));
        assert!(is_finance("my stock portfolio"));
        assert!(!is_finance("costs $5 per month"));
        assert!(!is_finance("too many API calls"));
        assert!(!is_finance("Rust and Cargo"));
    }

    #[test]
    fn respects_allowed_feeds() {
        let only_ai = |c: &str| c == "ai";
        assert_eq!(classify("AI agents", &only_ai), vec!["ai"]);
    }
}
