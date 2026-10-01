//! Word-level diff between the input and a rewrite.

use similar::{ChangeTag, TextDiff};

/// How a span differs between the old and new text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// In both.
    Same,
    /// Only in the new text.
    Added,
    /// Only in the old text.
    Removed,
}

/// A run of text with one kind of change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    /// The kind of change.
    pub change: Change,
    /// The text, including its whitespace.
    pub text: String,
}

/// Adjacent tokens with the same tag are merged into one span.
#[must_use]
pub fn word_diff(old: &str, new: &str) -> Vec<Span> {
    let diff = TextDiff::from_words(old, new);
    let mut out: Vec<Span> = Vec::new();
    for c in diff.iter_all_changes() {
        let change = match c.tag() {
            ChangeTag::Equal => Change::Same,
            ChangeTag::Insert => Change::Added,
            ChangeTag::Delete => Change::Removed,
        };
        match out.last_mut() {
            Some(last) if last.change == change => last.text.push_str(c.value()),
            _ => out.push(Span { change, text: c.value().to_owned() }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_replaced_word() {
        let d = word_diff("their going home", "they're going home");
        let removed: String = d.iter().filter(|s| s.change == Change::Removed).map(|s| s.text.as_str()).collect();
        let added: String = d.iter().filter(|s| s.change == Change::Added).map(|s| s.text.as_str()).collect();
        assert_eq!(removed, "their");
        assert_eq!(added, "they're");
    }

    #[test]
    fn reconstructs_both_sides() {
        let (a, b) = ("a b c d", "a x c d e");
        let d = word_diff(a, b);
        let old: String = d.iter().filter(|s| s.change != Change::Added).map(|s| s.text.as_str()).collect();
        let new: String = d.iter().filter(|s| s.change != Change::Removed).map(|s| s.text.as_str()).collect();
        assert_eq!((old.as_str(), new.as_str()), (a, b));
    }
}
