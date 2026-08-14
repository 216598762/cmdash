pub fn extract_urls(text: &str) -> Vec<String> {
    text.split_whitespace()
        .map(|token| {
            token.trim_matches(|character: char| {
                matches!(
                    character,
                    '(' | ')' | '[' | ']' | '{' | '}' | ',' | '.' | ';'
                )
            })
        })
        .filter(|token| token.starts_with("http://") || token.starts_with("https://"))
        .map(str::to_owned)
        .collect()
}

pub fn copy_notification(text: &str) -> String {
    match extract_urls(text).first() {
        Some(url) => format!("URL copied: {url}"),
        None => "selection copied".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_http_urls_without_trailing_punctuation() {
        assert_eq!(
            extract_urls("see https://example.test/path, then http://localhost."),
            [
                "https://example.test/path".to_owned(),
                "http://localhost".to_owned()
            ]
        );
    }

    #[test]
    fn formats_copy_notifications_for_urls_and_plain_text() {
        assert_eq!(
            copy_notification("https://example.test"),
            "URL copied: https://example.test"
        );
        assert_eq!(copy_notification("plain text"), "selection copied");
    }
}
