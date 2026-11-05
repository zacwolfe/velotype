//! Pure matcher for the find bar.
//!
//! This module is deliberately gpui-free so it can be unit tested directly.
//! It owns three concerns the find bar needs:
//!
//! - literal matching (case-sensitive or Unicode-case-folded),
//! - optional whole-word filtering,
//! - regular expression matching with capture-group expansion in replacements.
//!
//! Every returned range is a valid UTF-8 byte range in the *original* haystack,
//! which is what block `display_text()` offsets and
//! `replace_text_in_visible_range` both expect.

use std::fmt;
use std::ops::Range;

use regex::{Regex, RegexBuilder};

/// Search configuration options for the matcher.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SearchOptions {
    /// Only match text with exactly the same casing.
    pub case_sensitive: bool,
    /// Require a non-word character (or string edge) on both sides of a match.
    pub whole_word: bool,
    /// Interpret the query as a regular expression.
    pub regex: bool,
}

/// Why a matcher could not be built.
pub(super) enum MatcherError {
    /// The query was empty, which never matches anything.
    Empty,
    /// The regular expression failed to compile.
    BadRegex(String),
}

impl fmt::Display for MatcherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MatcherError::Empty => write!(f, "empty query"),
            MatcherError::BadRegex(message) => write!(f, "{message}"),
        }
    }
}

impl fmt::Debug for MatcherError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for MatcherError {}

/// A compiled query, ready to run against many haystacks.
#[derive(Clone, Debug)]
pub(super) enum Matcher {
    /// Plain substring matching, with case folding done by hand so that the
    /// returned ranges stay on the original string's byte boundaries.
    Literal {
        needle: String,
        needle_folded: String,
        case_sensitive: bool,
        whole_word: bool,
    },
    /// Regular expression matching (`multi_line`, `.` never matches `\n`).
    Regex(Regex),
}

impl Matcher {
    /// Compile `query` according to `options`.
    pub(super) fn build(query: &str, options: &SearchOptions) -> Result<Self, MatcherError> {
        if query.is_empty() {
            return Err(MatcherError::Empty);
        }

        if options.regex {
            let pattern = if options.whole_word {
                format!(r"\b(?:{query})\b")
            } else {
                query.to_string()
            };
            let mut builder = RegexBuilder::new(&pattern);
            // `^`/`$` should behave per line, like every mainstream find bar.
            builder.multi_line(true);
            builder.case_insensitive(!options.case_sensitive);
            builder
                .build()
                .map(Matcher::Regex)
                .map_err(|error| MatcherError::BadRegex(error.to_string()))
        } else {
            Ok(Matcher::Literal {
                needle: query.to_string(),
                needle_folded: fold(query),
                case_sensitive: options.case_sensitive,
                whole_word: options.whole_word,
            })
        }
    }

    /// All non-overlapping matches of the query in `haystack`, in ascending order.
    pub(super) fn find_all(&self, haystack: &str) -> Vec<Range<usize>> {
        match self {
            Matcher::Regex(regex) => regex
                .find_iter(haystack)
                .map(|found| found.start()..found.end())
                .filter(|range| !range.is_empty())
                .collect(),
            Matcher::Literal {
                needle,
                needle_folded,
                case_sensitive,
                whole_word,
            } => {
                if needle.is_empty() {
                    return Vec::new();
                }
                if *case_sensitive {
                    find_all_plain(haystack, needle, *whole_word)
                } else {
                    find_all_folded(haystack, needle_folded, *whole_word)
                }
            }
        }
    }

    /// Expand `replacement` for the text matched at `range` inside `haystack`.
    ///
    /// `$1` / `${name}` are only meaningful in regex mode; in literal mode a
    /// `$` is an ordinary character, which is what users expect when they are
    /// not using a regular expression.
    pub(super) fn expand(&self, replacement: &str, haystack: &str, range: Range<usize>) -> String {
        match self {
            Matcher::Literal { .. } => replacement.to_string(),
            Matcher::Regex(regex) => {
                let Some(slice) = haystack.get(range) else {
                    return replacement.to_string();
                };
                let Some(captures) = regex.captures(slice) else {
                    return replacement.to_string();
                };
                let mut expanded = String::with_capacity(replacement.len());
                captures.expand(replacement, &mut expanded);
                expanded
            }
        }
    }
}

fn find_all_plain(haystack: &str, needle: &str, whole_word: bool) -> Vec<Range<usize>> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while let Some(offset) = haystack[start..].find(needle) {
        let match_start = start + offset;
        let match_end = match_start + needle.len();
        if !whole_word || is_whole_word(haystack, match_start, match_end) {
            ranges.push(match_start..match_end);
        }
        start = match_end;
    }
    ranges
}

fn find_all_folded(haystack: &str, needle_folded: &str, whole_word: bool) -> Vec<Range<usize>> {
    if needle_folded.is_empty() {
        return Vec::new();
    }
    let (folded, source_offsets) = fold_with_map(haystack);
    let mut ranges = Vec::new();
    let mut start = 0;
    while let Some(offset) = folded[start..].find(needle_folded) {
        let folded_start = start + offset;
        let folded_end = folded_start + needle_folded.len();
        start = folded_end;

        // `folded` byte N maps back to the byte offset of the source char that
        // produced it; the range end is that char's own end offset.
        let Some(&source_start) = source_offsets.get(folded_start) else {
            continue;
        };
        let Some(&last_source_start) = source_offsets.get(folded_end - 1) else {
            continue;
        };
        let source_end = last_source_start + char_len_at(haystack, last_source_start);
        if source_start >= source_end {
            continue;
        }
        if whole_word && !is_whole_word(haystack, source_start, source_end) {
            continue;
        }
        ranges.push(source_start..source_end);
    }
    ranges
}

fn char_len_at(haystack: &str, offset: usize) -> usize {
    haystack[offset..]
        .chars()
        .next()
        .map(char::len_utf8)
        .unwrap_or(0)
}

fn fold(text: &str) -> String {
    let mut folded = String::with_capacity(text.len());
    for character in text.chars() {
        folded.extend(character.to_lowercase());
    }
    folded
}

/// Fold `text` and record, for every byte of the folded string, the byte offset
/// in `text` of the character that produced it.
///
/// Folding can change byte length (`İ` folds to `i` + combining dot), which is
/// why a simple `to_lowercase()` on the haystack cannot be indexed with offsets
/// from the original string. This map makes the conversion exact.
fn fold_with_map(text: &str) -> (String, Vec<usize>) {
    let mut folded = String::with_capacity(text.len());
    let mut source_offsets = Vec::with_capacity(text.len());
    for (offset, character) in text.char_indices() {
        let start = folded.len();
        for lower in character.to_lowercase() {
            folded.push(lower);
        }
        let pushed = folded.len() - start;
        source_offsets.extend(std::iter::repeat_n(offset, pushed));
    }
    (folded, source_offsets)
}

fn is_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn is_whole_word(haystack: &str, start: usize, end: usize) -> bool {
    let before = haystack
        .get(..start)
        .and_then(|head| head.chars().next_back());
    let after = haystack.get(end..).and_then(|tail| tail.chars().next());
    !before.is_some_and(is_word_char) && !after.is_some_and(is_word_char)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn literal(case_sensitive: bool, whole_word: bool) -> SearchOptions {
        SearchOptions {
            case_sensitive,
            whole_word,
            regex: false,
        }
    }

    fn regex_options(whole_word: bool) -> SearchOptions {
        SearchOptions {
            case_sensitive: false,
            whole_word,
            regex: true,
        }
    }

    #[test]
    fn literal_search_is_case_insensitive_by_default() {
        let matcher = Matcher::build("foo", &SearchOptions::default()).unwrap();
        assert_eq!(matcher.find_all("Foo foo FOO bar"), vec![0..3, 4..7, 8..11]);
    }

    #[test]
    fn literal_search_honours_case_sensitive_flag() {
        let matcher = Matcher::build("foo", &literal(true, false)).unwrap();
        assert_eq!(matcher.find_all("Foo foo FOO bar"), vec![4..7]);
    }

    #[test]
    fn unicode_folding_matches_non_ascii_case_pairs() {
        let matcher = Matcher::build("ärger", &SearchOptions::default()).unwrap();
        let ranges = matcher.find_all("Ärger und ÄRGER");
        assert_eq!(ranges, vec![0..6, 11..17]);

        // Greek sigma folds both ways.
        let sigma = Matcher::build("σ", &SearchOptions::default()).unwrap();
        let sigma_ranges = sigma.find_all("Σίγμα");
        assert_eq!(sigma_ranges, vec![0..2]);
    }

    #[test]
    fn unicode_folding_ranges_stay_on_char_boundaries() {
        let haystack = "İstanbul und ÄRGER";
        let matcher = Matcher::build("ärger", &SearchOptions::default()).unwrap();
        let ranges = matcher.find_all(haystack);
        assert_eq!(ranges.len(), 1);
        for range in &ranges {
            assert!(
                haystack.is_char_boundary(range.start) && haystack.is_char_boundary(range.end),
                "range {range:?} is not on a char boundary"
            );
        }
        assert_eq!(&haystack[ranges[0].clone()], "ÄRGER");

        // `İ` folds to two characters; the mapped range must still cover the
        // whole original character rather than half of it.
        let dotted = Matcher::build("i̇stanbul", &SearchOptions::default()).unwrap();
        let dotted_ranges = dotted.find_all(haystack);
        // The whole word matches, and the mapped range covers all 9 bytes of
        // the original `İstanbul` (the 2-byte `İ` expands to 3 folded bytes).
        assert_eq!(dotted_ranges, vec![0..9]);
        assert_eq!(&haystack[0..9], "İstanbul");
        assert!(haystack.is_char_boundary(dotted_ranges[0].start));
        assert!(haystack.is_char_boundary(dotted_ranges[0].end));
    }

    #[test]
    fn whole_word_rejects_substring_inside_word() {
        let matcher = Matcher::build("the", &literal(false, true)).unwrap();
        assert_eq!(matcher.find_all("there is the theory"), vec![9..12]);
    }

    #[test]
    fn whole_word_accepts_punctuation_bounded_match() {
        let matcher = Matcher::build("the", &literal(false, true)).unwrap();
        assert_eq!(matcher.find_all("the (the) the."), vec![0..3, 5..8, 10..13]);
    }

    #[test]
    fn whole_word_works_on_case_sensitive_searches() {
        let matcher = Matcher::build("The", &literal(true, true)).unwrap();
        assert_eq!(
            matcher.find_all("The theory of the The."),
            vec![0..3, 18..21]
        );
    }

    #[test]
    fn regex_matches_capture_groups_and_expands_dollar_refs() {
        let matcher = Matcher::build(r"(\d{4})", &regex_options(false)).unwrap();
        assert_eq!(matcher.find_all("2021 and 2022"), vec![0..4, 9..13]);
        assert_eq!(matcher.expand("y$1", "2021 and 2022", 0..4), "y2021");
        assert_eq!(matcher.expand("y$1", "2021 and 2022", 9..13), "y2022");
    }

    #[test]
    fn literal_replacement_does_not_expand_dollar_refs() {
        let matcher = Matcher::build("foo", &SearchOptions::default()).unwrap();
        assert_eq!(matcher.expand("$1bar", "foo", 0..3), "$1bar");
    }

    #[test]
    fn regex_is_multi_line_but_dot_does_not_match_newline() {
        let anchors = Matcher::build("^b", &regex_options(false)).unwrap();
        assert_eq!(anchors.find_all("a\nb"), vec![2..3]);

        let dot = Matcher::build("a.b", &regex_options(false)).unwrap();
        assert!(dot.find_all("a\nb").is_empty());
    }

    #[test]
    fn regex_honours_the_case_sensitive_flag() {
        let insensitive = Matcher::build("foo", &regex_options(false)).unwrap();
        assert_eq!(insensitive.find_all("FOO foo"), vec![0..3, 4..7]);

        let sensitive = Matcher::build(
            "foo",
            &SearchOptions {
                case_sensitive: true,
                ..regex_options(false)
            },
        )
        .unwrap();
        assert_eq!(sensitive.find_all("FOO foo"), vec![4..7]);
    }

    #[test]
    fn regex_whole_word_wraps_the_pattern() {
        let matcher = Matcher::build("the", &regex_options(true)).unwrap();
        assert_eq!(matcher.find_all("there is the theory"), vec![9..12]);
    }

    #[test]
    fn invalid_regex_returns_error_without_panicking() {
        let error = Matcher::build("[", &regex_options(false)).unwrap_err();
        assert!(matches!(error, MatcherError::BadRegex(_)));
        assert!(!error.to_string().is_empty());
    }

    #[test]
    fn empty_query_matches_nothing() {
        assert!(matches!(
            Matcher::build("", &SearchOptions::default()),
            Err(MatcherError::Empty)
        ));
        let matcher = Matcher::build("test", &SearchOptions::default()).unwrap();
        assert!(matcher.find_all("").is_empty());
    }

    #[test]
    fn matches_never_overlap() {
        let matcher = Matcher::build("aa", &SearchOptions::default()).unwrap();
        assert_eq!(matcher.find_all("aaaa"), vec![0..2, 2..4]);
    }
}
