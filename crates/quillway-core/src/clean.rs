//! The model's output as shown: what it wrote, minus protocol artifacts.
//!
//! No heuristics: with the prompt's instructions and few-shot turns the models
//! don't wrap rewrites in chat (0 of 250 outputs needed it; see
//! `docs/research/model-eval.md`), and rules guessing at "chatter" deleted real
//! content such as an email's closing line.

/// `raw` as far as it has arrived, for display while streaming: without a
/// `<think>…</think>` block (an unterminated one hides everything after it).
#[must_use]
pub fn visible(raw: &str) -> &str {
    let t = raw.trim_start();
    let Some(rest) = t.strip_prefix("<think>") else { return t };
    rest.find("</think>").map_or("", |i| rest[i + "</think>".len()..].trim_start())
}

/// The finished output: [`visible`], trimmed, ending with a newline if `input` did.
#[must_use]
pub fn clean(raw: &str, input: &str) -> String {
    let mut out = visible(raw).trim_end().to_owned();
    if input.ends_with("\r\n") {
        out.push_str("\r\n");
    } else if input.ends_with('\n') {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_output_through_unchanged() {
        assert_eq!(clean("They're here.", "their here"), "They're here.");
        // No guessing: a lead-in or closing line the model wrote is kept.
        let email = "Here is the plan:\n- Ship\n\nLet me know if you have any questions.";
        assert_eq!(clean(email, "plan: ship\n\nlmk if questions"), email);
    }

    #[test]
    fn strips_think_blocks() {
        assert_eq!(clean("<think>\n\n</think>\n\nHello.", "hello"), "Hello.");
        assert_eq!(visible("<think>still going"), "");
        assert_eq!(visible("<think>done</think> Hi"), "Hi");
    }

    #[test]
    fn mirrors_the_trailing_newline() {
        assert_eq!(clean("Hello.\n\n", "hello\n"), "Hello.\n");
        assert_eq!(clean("Hello.", "hello\r\n"), "Hello.\r\n");
        assert_eq!(clean("Hello.\n", "hello"), "Hello.");
    }
}
