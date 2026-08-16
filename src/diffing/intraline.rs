//! Word-level intraline highlighting: for a [`crate::diffing::hunks::LinePair::Changed`]
//! pair, [`intraline`] finds which byte ranges of each line actually
//! differ, so the diff pane can paint a stronger highlight over just the
//! changed words rather than the whole line. Pure -- no I/O.

use imara_diff::{Algorithm, Diff, InternedInput, TokenSource};

/// A byte range within a line that differs from its paired line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HighlightSpan {
    pub start: usize,
    pub end: usize,
}

/// Diff `base_line` against `head_line` at word granularity (see
/// [`tokenize_ranges`] for the tokenization rule), returning the byte
/// spans that changed in each: `(spans in base_line, spans in head_line)`.
/// A line that's entirely different from its pair comes back as a single
/// span covering the whole line, since tokenization has no gaps between
/// tokens.
pub fn intraline(base_line: &str, head_line: &str) -> (Vec<HighlightSpan>, Vec<HighlightSpan>) {
    let base_ranges = tokenize_ranges(base_line);
    let head_ranges = tokenize_ranges(head_line);

    let base_tokens: Vec<&str> = base_ranges.iter().map(|&(s, e)| &base_line[s..e]).collect();
    let head_tokens: Vec<&str> = head_ranges.iter().map(|&(s, e)| &head_line[s..e]).collect();

    let input = InternedInput::new(Tokens(base_tokens), Tokens(head_tokens));
    let diff = Diff::compute(Algorithm::Histogram, &input);

    let mut base_spans = Vec::new();
    let mut head_spans = Vec::new();
    for hunk in diff.hunks() {
        if !hunk.before.is_empty() {
            let start = base_ranges[hunk.before.start as usize].0;
            let end = base_ranges[hunk.before.end as usize - 1].1;
            base_spans.push(HighlightSpan { start, end });
        }
        if !hunk.after.is_empty() {
            let start = head_ranges[hunk.after.start as usize].0;
            let end = head_ranges[hunk.after.end as usize - 1].1;
            head_spans.push(HighlightSpan { start, end });
        }
    }
    (base_spans, head_spans)
}

/// A thin [`TokenSource`] wrapper over a pre-split `Vec<&str>` of word/
/// whitespace/punctuation tokens (see [`tokenize_ranges`]), so `imara-diff`
/// can run at token granularity instead of its built-in line granularity.
struct Tokens<'a>(Vec<&'a str>);

impl<'a> TokenSource for Tokens<'a> {
    type Token = &'a str;
    type Tokenizer = std::vec::IntoIter<&'a str>;

    fn tokenize(&self) -> Self::Tokenizer {
        self.0.clone().into_iter()
    }

    fn estimate_tokens(&self) -> u32 {
        self.0.len() as u32
    }
}

/// Which token class a character belongs to, for [`tokenize_ranges`].
#[derive(PartialEq, Eq)]
enum CharClass {
    Word,
    Space,
    Other,
}

fn classify(c: char) -> CharClass {
    if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Other
    }
}

/// Split `line` into byte ranges of maximal runs of the same [`CharClass`]:
/// word characters (alphanumeric/`_`), whitespace, or punctuation/other.
/// Adjacent tokens abut with no gaps, so joining every range back together
/// reconstructs `line` exactly.
fn tokenize_ranges(line: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut iter = line.char_indices();
    let Some((first_start, first_char)) = iter.next() else {
        return ranges;
    };

    let mut start = first_start;
    let mut cur_class = classify(first_char);
    let mut end = first_start + first_char.len_utf8();

    for (i, c) in iter {
        let class = classify(c);
        if class == cur_class {
            end = i + c.len_utf8();
        } else {
            ranges.push((start, end));
            start = i;
            cur_class = class;
            end = i + c.len_utf8();
        }
    }
    ranges.push((start, end));
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_lines_have_no_spans() {
        let (base, head) = intraline("let x = 1;", "let x = 1;");
        assert!(base.is_empty());
        assert!(head.is_empty());
    }

    #[test]
    fn one_word_changed_mid_line() {
        let (base, head) = intraline("let x = one;", "let x = two;");
        assert_eq!(base.len(), 1);
        assert_eq!(head.len(), 1);
        assert_eq!(&"let x = one;"[base[0].start..base[0].end], "one");
        assert_eq!(&"let x = two;"[head[0].start..head[0].end], "two");
    }

    #[test]
    fn prefix_and_suffix_unchanged() {
        let (base, head) = intraline("prefix middle suffix", "prefix changed suffix");
        assert_eq!(base.len(), 1);
        assert_eq!(head.len(), 1);
        assert_eq!(
            &"prefix middle suffix"[base[0].start..base[0].end],
            "middle"
        );
        assert_eq!(
            &"prefix changed suffix"[head[0].start..head[0].end],
            "changed"
        );
    }

    #[test]
    fn fully_different_lines_return_full_line_spans() {
        let base_line = "abc";
        let head_line = "xyz123";
        let (base, head) = intraline(base_line, head_line);
        assert_eq!(
            base,
            vec![HighlightSpan {
                start: 0,
                end: base_line.len()
            }]
        );
        assert_eq!(
            head,
            vec![HighlightSpan {
                start: 0,
                end: head_line.len()
            }]
        );
    }
}
