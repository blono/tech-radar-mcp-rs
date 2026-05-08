use html_escape::decode_html_entities;
use sha2::{Digest, Sha256};

/// RSS / Atom の summary に含まれがちな簡単な HTML tag を落とします。
///
/// これは厳密な HTML sanitizer ではありません。
/// ここでは「feed summary を LLM に渡しやすい短い text にする」目的に限定しています。
///
/// 将来、任意 URL の本文 snapshot を扱う場合は、この関数では足りません。
/// その場合は HTML parser / sanitizer / allowlist / max bytes / excerpt 制限を別途入れてください。
pub fn clean_summary(value: &str, max_chars: usize) -> String {
    let mut result = String::with_capacity(value.len());
    let mut in_tag = false;

    for ch in value.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => result.push(ch),
            _ => {}
        }
    }

    let result = decode_html_entities(&result)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    truncate_chars(&result, max_chars)
}

/// Unicode 境界を壊さないように文字数で切り詰めます。
pub fn truncate_chars(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }

    let mut out = value.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}

/// URL がない feed item 用の安定 ID を作るために使います。
pub fn sha256_hex(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_summary_strips_simple_tags() {
        let input = "<p>Hello&nbsp;<strong>world</strong></p>";
        assert_eq!(clean_summary(input, 100), "Hello world");
    }

    #[test]
    fn truncate_works_on_unicode_boundary() {
        assert_eq!(truncate_chars("あいうえお", 3), "あいう...");
    }
}
