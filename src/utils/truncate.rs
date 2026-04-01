/// Safely truncate a string to at most `max` bytes, respecting UTF-8
/// character boundaries.  Appends `suffix` (default "…") when truncated.
///
/// This avoids panics when slicing multi-byte characters (emojis, CJK, etc.).
pub fn safe_truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let truncate_at = s
        .char_indices()
        .take_while(|&(i, _)| i < max.saturating_sub(3)) // reserve space for "…"
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}…", &s[..truncate_at])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_string_unchanged() {
        assert_eq!(safe_truncate("hello", 10), "hello");
    }

    #[test]
    fn test_long_string_truncated() {
        let result = safe_truncate("hello world!", 8);
        assert!(result.len() <= 10); // 8 bytes + "…" (3 bytes UTF-8)
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_emoji_boundary_safe() {
        // 🔴 is 4 bytes (U+1F534). Truncating at byte 3 would be mid-character.
        let s = "ab🔴cd";
        let result = safe_truncate(s, 5);
        // Should not panic, and should truncate before or at the emoji.
        assert!(result.ends_with('…'));
        assert!(!result.contains('🔴')); // emoji doesn't fit in 5 bytes
    }

    #[test]
    fn test_exact_length() {
        assert_eq!(safe_truncate("12345", 5), "12345");
    }

    #[test]
    fn test_empty_string() {
        assert_eq!(safe_truncate("", 10), "");
    }

    #[test]
    fn test_cjk_characters() {
        // Each CJK character is 3 bytes.
        let s = "你好世界"; // 12 bytes
        let result = safe_truncate(s, 8);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn test_max_zero() {
        assert_eq!(safe_truncate("hello", 0), "…");
    }
}
