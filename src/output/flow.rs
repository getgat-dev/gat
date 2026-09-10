//! Lossless word placement for approved logical lines. Protected identities are
//! atomic, including internal spaces. Oversized words and fixed prefixes may
//! exceed the soft width. ANSI styling belongs to the caller, after placement.
use crate::presentation::UserLine;
use unicode_width::UnicodeWidthStr;

/// Placement retains the boundary between renderer-owned prefixes and approved
/// content. Styling must never rediscover that boundary by searching body text.
pub(super) struct PhysicalLine<'a> {
    pub prefix: &'a str,
    pub body: String,
}

impl std::fmt::Display for PhysicalLine<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", self.prefix, self.body)
    }
}

/// Yield one physical line at a time. Prefixes are plain presentation text.
/// When a prefix consumes the available width, put it on its own line and
/// reduce continuation indentation rather than inventing a wider terminal.
pub(super) fn lines<'a>(
    first: &'a str,
    continuation: &'a str,
    text: &'a UserLine,
    width: usize,
) -> impl Iterator<Item = PhysicalLine<'a>> + 'a {
    let width = width.max(1);
    let mut words = text
        .wrapping_words()
        .map(|word| (word, word.width()))
        .peekable();
    let continuation = if continuation.width() >= width {
        ""
    } else {
        continuation
    };
    let continuation_width = continuation.width();
    let mut prefix = Some(first);
    std::iter::from_fn(move || {
        if prefix.is_none() && words.peek().is_none() {
            return None;
        }
        let first_line = prefix.is_some();
        let prefix = prefix.take().unwrap_or(continuation);
        let mut body = String::new();
        let mut columns = prefix.width();
        let mut has_word = false;
        while let Some(&(word, word_width)) = words.peek() {
            let needed = usize::from(has_word) + word_width;
            if columns + needed > width
                && (has_word
                    || (first_line
                        && !prefix.trim().is_empty()
                        && (columns >= width || continuation_width + word_width <= width)))
            {
                break;
            }
            if has_word {
                body.push(' ');
            }
            body.push_str(word);
            columns += needed;
            has_word = true;
            words.next();
        }
        Some(PhysicalLine {
            prefix: if has_word { prefix } else { prefix.trim_end() },
            body,
        })
    })
}

/// Collect physical lines in layout fixtures.
#[cfg(test)]
pub(super) fn wrap(first: &str, continuation: &str, text: &UserLine, width: usize) -> String {
    lines(first, continuation, text, width)
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_values_move_intact_and_keep_internal_spaces() {
        let text = UserLine::compose([
            UserLine::authored("Could not read "),
            UserLine::identifier("a folder/file"),
        ]);
        assert_eq!(
            wrap("! ", "  ", &text, 18),
            "! Could not read\n  a folder/file"
        );
    }

    #[test]
    fn narrow_prefixes_do_not_force_continuation_indentation() {
        assert_eq!(
            wrap("hint: ", "      ", &UserLine::authored("a b"), 3),
            "hint:\na b"
        );
        for width in [0, 1, 2, 3, 8, 39, 40, 80, 100] {
            let value = UserLine::identifier("日本語 with spaces\n\u{1b}");
            let rendered = wrap("", "", &value, width);
            assert_eq!(rendered, value.as_str());
        }
    }
}
