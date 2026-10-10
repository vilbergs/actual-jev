//! Note tags are the only state the tool keeps in Actual.
//!
//! A note ends up as `<original text> #jev` or `<original text> #jev-flag [jev: Groceries 0.71]`.
//! Stripping removes every `#<prefix>…` token and every `[jev: …]` segment, so a rerun
//! never stacks tags.

/// Removes previous tags with the given prefix and any `[jev: …]` guess segment.
pub fn strip(notes: &str, prefix: &str) -> String {
    let without_guess = remove_segments(notes, "[jev:", ']');
    without_guess
        .split_whitespace()
        .filter(|token| !token.starts_with(prefix))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Appends `tag` (and an optional guess segment) to notes stripped of earlier tags.
pub fn with_tag(notes: &str, prefix: &str, tag: &str, guess: Option<&str>) -> String {
    let mut out = strip(notes, prefix);
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(tag);
    if let Some(guess) = guess {
        out.push_str(" [jev: ");
        out.push_str(guess);
        out.push(']');
    }
    out
}

pub fn has_tag(notes: &str, tag: &str) -> bool {
    notes.split_whitespace().any(|token| token == tag)
}

fn remove_segments(text: &str, start: &str, end: char) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(begin) = rest.find(start) {
        out.push_str(&rest[..begin]);
        match rest[begin..].find(end) {
            Some(offset) => rest = &rest[begin + offset + end.len_utf8()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_tags_and_guess_segments() {
        assert_eq!(strip("Weekly shop #jev", "#jev"), "Weekly shop");
        assert_eq!(
            strip("Weekly shop #jev-flag [jev: Groceries 0.71]", "#jev"),
            "Weekly shop"
        );
        assert_eq!(strip("#jev-miss", "#jev"), "");
        assert_eq!(strip("keep #other", "#jev"), "keep #other");
    }

    #[test]
    fn with_tag_never_stacks() {
        let once = with_tag("Lunch", "#jev", "#jev-flag", Some("Restaurants 0.70"));
        assert_eq!(once, "Lunch #jev-flag [jev: Restaurants 0.70]");
        let twice = with_tag(&once, "#jev", "#jev", None);
        assert_eq!(twice, "Lunch #jev");
        assert_eq!(with_tag("", "#jev", "#jev-miss", None), "#jev-miss");
    }

    #[test]
    fn has_tag_matches_whole_tokens_only() {
        assert!(has_tag("a #jev-miss", "#jev-miss"));
        assert!(!has_tag("a #jev-missing", "#jev-miss"));
        assert!(!has_tag("a #jev", "#jev-miss"));
    }
}
