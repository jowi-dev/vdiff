//! Direct `syntect` -> `ratatui::style::Style` glue for the file and diff
//! views. The GUI reaches syntax highlighting through `egui_extras`'s
//! `syntax_highlighting` module, which hands back an egui `LayoutJob` --
//! useless here, since this build may have no egui at all. This module
//! does the same `syntect` highlighting pass directly and maps its color/
//! font-style output onto `ratatui`'s span styling instead.
//!
//! Accepted phase-1 limitation: [`highlight_line`] constructs a fresh
//! `HighlightLines` per call, so its parse state resets every line rather
//! than carrying forward across the file the way a real syntax-aware
//! editor would; multi-line constructs (block comments, raw strings,
//! multi-line f-strings, ...) can highlight incorrectly as a result. A
//! correct fix means threading one `HighlightLines` per rendered file
//! across calls instead of building one per line, which is deferred to a
//! later pass.

use std::path::Path;
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{Color as SynColor, FontStyle, Style as SynStyle, Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

/// The bundled default syntax/theme sets, loaded once per process --
/// `syntect`'s own loaders parse a nontrivial amount of packaged data, so
/// this is cached rather than redone per file/line.
fn syntax_set() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// `base16-ocean.dark` -- a theme bundled with every `syntect` install (no
/// extra asset download), and dark to match the terminal's own default
/// background in the overwhelming majority of setups this tool runs in.
fn theme() -> &'static Theme {
    static THEME: OnceLock<Theme> = OnceLock::new();
    THEME.get_or_init(|| {
        let mut set = ThemeSet::load_defaults();
        set.themes
            .remove("base16-ocean.dark")
            .expect("syntect bundles base16-ocean.dark by default")
    })
}

/// Highlight one line of source, keyed by `path`'s extension (falls back to
/// plain, unstyled text for an unrecognized/missing extension -- never an
/// error; a file view with no highlighting is still fully usable). Returns
/// owned [`Span`]s (`'static`) since callers build these fresh per frame
/// from a `String` they don't keep borrowed.
pub fn highlight_line(path: &Path, line: &str) -> Vec<Span<'static>> {
    let set = syntax_set();
    let syntax = syntax_for(set, path);
    let mut highlighter = HighlightLines::new(syntax, theme());
    // `syntect` expects each call's line to end in `\n` when the syntax set
    // was loaded with `load_defaults_newlines` (it is, per `syntax_set`) --
    // callers here always pass one already-split line with no trailing
    // newline, so one is appended for the call and never carried into the
    // rendered span text itself.
    let mut owned_line = line.to_string();
    owned_line.push('\n');
    let ranges: Vec<(SynStyle, &str)> = highlighter
        .highlight_line(&owned_line, set)
        .unwrap_or_else(|_| vec![(SynStyle::default(), line)]);

    ranges
        .into_iter()
        .map(|(style, text)| {
            Span::styled(
                text.trim_end_matches('\n').to_string(),
                to_ratatui_style(style),
            )
        })
        .collect()
}

/// The syntax to highlight `path` with, by extension, falling back to
/// syntect's built-in plain-text syntax (never `None` -- every `SyntaxSet`
/// carries one).
fn syntax_for<'a>(set: &'a SyntaxSet, path: &Path) -> &'a SyntaxReference {
    path.extension()
        .and_then(|ext| ext.to_str())
        .and_then(|ext| set.find_syntax_by_extension(ext))
        .unwrap_or_else(|| set.find_syntax_plain_text())
}

/// Map one `syntect` highlight range's style onto a `ratatui::style::Style`:
/// foreground color always (syntect themes always set one), bold/italic/
/// underline mapped from [`FontStyle`]. Background is deliberately left
/// alone -- painting every span's own theme background would fight the
/// terminal's own background color and any selection/cursor highlighting
/// `ratatui` widgets add on top, for no real readability gain in a 16/256-
/// color terminal palette.
fn to_ratatui_style(style: SynStyle) -> Style {
    let mut out = Style::default().fg(to_ratatui_color(style.foreground));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

fn to_ratatui_color(color: SynColor) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_line_recognizes_a_rust_extension() {
        let spans = highlight_line(Path::new("foo.rs"), "fn main() {}");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "fn main() {}");
    }

    #[test]
    fn highlight_line_falls_back_to_plain_text_for_unknown_extension() {
        let spans = highlight_line(Path::new("foo.made-up-extension"), "hello world");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "hello world");
    }

    #[test]
    fn highlight_line_handles_empty_input() {
        let spans = highlight_line(Path::new("foo.rs"), "");
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "");
    }
}
