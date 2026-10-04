//! Full-text-search term construction.
//!
//! Evidence: MIGRATION_NOTES.md "Search (§2.9)" (`match_terms`:
//! whitespace/NUL-split, each word double-quoted with `"` escaped — AND
//! semantics, no query syntax, no `NOT/AND/OR/NEAR` injection — and a query
//! that sanitizes to only spaces yields empty `match_terms` → empty results,
//! NOT an error).

/// Upstream `SearchesController#query`: non-word characters become
/// spaces (`/[^[:word:]]/`; word = alphanumeric + underscore).
pub fn sanitize_query(query: &str) -> String {
    query
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                ' '
            }
        })
        .collect()
}

/// Build the FTS `MATCH` expression for a sanitized query: split on
/// whitespace and NUL, drop empties, wrap each remaining word in double
/// quotes (doubling any embedded `"`), and space-join the results.
///
/// Quoting every term gives AND semantics while disabling query syntax, so
/// words like `AND`, `OR`, or `NOT` can never act as operators. An input
/// with no words (empty, whitespace-only, or NUL-only) yields an empty
/// string, which the caller treats as empty results — never an error.
pub fn match_terms(query: &str) -> String {
    query
        .split(|c: char| c == '\0' || c.is_whitespace())
        .filter(|word| !word.is_empty())
        .map(|word| format!("\"{}\"", word.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_word_is_quoted() {
        assert_eq!(match_terms("hello"), "\"hello\"");
    }

    #[test]
    fn multiple_words_are_quoted_and_space_joined() {
        assert_eq!(match_terms("hello world"), "\"hello\" \"world\"");
    }

    #[test]
    fn extra_whitespace_collapses_without_empties() {
        // Evidence: whitespace-split with empties dropped.
        assert_eq!(match_terms("  hello   world  "), "\"hello\" \"world\"");
        assert_eq!(
            match_terms("hello\tworld\nnext"),
            "\"hello\" \"world\" \"next\""
        );
    }

    #[test]
    fn nul_splits_like_whitespace() {
        assert_eq!(match_terms("hello\0world"), "\"hello\" \"world\"");
        assert_eq!(match_terms("hello\0\0world"), "\"hello\" \"world\"");
    }

    #[test]
    fn empty_and_blank_inputs_yield_no_terms() {
        // Evidence: query that is only non-word chars sanitizes to spaces →
        // `match_terms` empty → empty results, NOT an error.
        assert_eq!(match_terms(""), "");
        assert_eq!(match_terms("   "), "");
        assert_eq!(match_terms("\0"), "");
        assert_eq!(match_terms(" \t\0\n "), "");
    }

    #[test]
    fn embedded_quotes_are_doubled() {
        assert_eq!(match_terms("sa\"y"), "\"sa\"\"y\"");
        assert_eq!(match_terms("\""), "\"\"\"\"");
    }

    #[test]
    fn query_operators_stay_quoted_without_syntax() {
        // Evidence: no `NOT/AND/OR/NEAR` injection.
        assert_eq!(
            match_terms("hello AND world"),
            "\"hello\" \"AND\" \"world\""
        );
        assert_eq!(match_terms("NOT OR NEAR"), "\"NOT\" \"OR\" \"NEAR\"");
    }

    #[test]
    fn unicode_words_survive() {
        // Evidence: `café_1 日本` survives the upstream sanitizer as words.
        assert_eq!(match_terms("café_1 日本"), "\"café_1\" \"日本\"");
    }
}
