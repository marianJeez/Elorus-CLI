//! Finding a model by what the list shows: id, display name, provider, description and the
//! capabilities column's words.

use crate::api::models::Model;

/// Everything a query is matched against for one model.
pub fn words(model: &Model) -> String {
    let mut text = format!(
        "{} {} {} {}",
        model.id, model.display_name, model.provider, model.description
    );
    let capabilities = &model.capabilities;
    for (yes, word) in [
        (capabilities.reasoning, "reasoning"),
        (capabilities.tool_calling, "tools"),
        (capabilities.caching, "caching"),
    ] {
        if yes {
            text.push(' ');
            text.push_str(word);
        }
    }
    text
}

/// Whether every word in `query` appears in `text`, ignoring case; an empty query matches.
pub fn matches_words(text: &str, query: &str) -> bool {
    let text = text.to_lowercase();
    query
        .split_whitespace()
        .all(|word| text.contains(&word.to_lowercase()))
}

/// Whether `model` matches `query`; an empty query matches everything.
pub fn matches(model: &Model, query: &str) -> bool {
    matches_words(&words(model), query)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_every_word_the_table_shows() {
        let model = crate::testing::model(true);
        for query in ["m", "elorus", "reasoning", "m elorus"] {
            assert!(matches(&model, query), "{query}");
        }
        assert!(!matches(&model, "nope"));
        assert!(!matches(&model, "m nope"));
        assert!(matches(&model, "  "));
    }
}
