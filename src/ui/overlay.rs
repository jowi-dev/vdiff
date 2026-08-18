//! Fullscreen editor overlay: when [`Pane::File`] is the active pane, this
//! paints on top of the already-rendered graph -- a scrim over the whole
//! viewport, then an opaque header strip, then the editor content (the nvim
//! grid, or the built-in [`file_view`] as a fallback) below it.
//!
//! This replaced a resizable 50%-width side panel. Rationale (decided with
//! the user): the graph and the editor answer different questions, and
//! splitting the viewport taxes both -- fullscreen nvim also fixes
//! plugins/UIs that feel cramped at half width. The scrim is a continuity
//! cue ("the graph is still there, dimmed"), not an information channel --
//! see [`theme::OVERLAY_SCRIM_ALPHA`]'s doc for why it leans nearly opaque
//! rather than something you're meant to read through.

use egui::{Align2, FontId, Pos2, Rect, Ui, UiBuilder, Vec2};

use crate::core::app::App;
use crate::core::file_view::{FileViewEntry, FileViewState};
use crate::graph::model::NodeId;
use crate::ui::file_view;
use crate::ui::nvim_pane::{self, NvimPane};
use crate::ui::theme;

/// Height of the opaque header strip.
const HEADER_HEIGHT: f32 = 28.0;

/// Horizontal padding before the header text.
const HEADER_PADDING: f32 = 8.0;

/// Paint the fullscreen overlay into `ui`'s current `max_rect` (the whole
/// viewport -- the graph has already been painted underneath by the
/// caller): scrim, header strip, then editor content in whatever's left.
/// `nvim` selects the content: `Some` paints the live nvim grid via
/// [`nvim_pane::show`]; `None` falls back to the built-in
/// [`file_view::show`]. Returns the row count the built-in viewer fit into
/// its content area (for [`App::viewport_rows`]'s `Ctrl-d`/`Ctrl-u`
/// half-page math) -- `None` in nvim mode, which doesn't use it.
pub fn show(
    ui: &mut Ui,
    app: &App,
    file_view_state: &FileViewState,
    nvim: Option<&mut NvimPane>,
) -> Option<usize> {
    let screen_rect = ui.max_rect();
    ui.painter()
        .rect_filled(screen_rect, 0.0, theme::overlay_scrim_color());

    let header_rect = Rect::from_min_size(
        screen_rect.min,
        Vec2::new(screen_rect.width(), HEADER_HEIGHT),
    );
    ui.painter()
        .rect_filled(header_rect, 0.0, theme::OVERLAY_HEADER_BG);

    let text = header_text(app, file_view_state, nvim.is_some());
    ui.painter().text(
        header_rect.left_center() + Vec2::new(HEADER_PADDING, 0.0),
        Align2::LEFT_CENTER,
        text,
        FontId::proportional(14.0),
        theme::OVERLAY_HEADER_TEXT,
    );

    let content_rect = Rect::from_min_max(
        Pos2::new(screen_rect.left(), header_rect.bottom()),
        screen_rect.max,
    );
    let mut content_ui = ui.new_child(UiBuilder::new().max_rect(content_rect));

    match nvim {
        Some(nvim_pane) => {
            nvim_pane::show(&mut content_ui, nvim_pane);
            None
        }
        None => Some(file_view::show(&mut content_ui, file_view_state)),
    }
}

/// Assemble the header strip's text for `app`'s current focus/layers and
/// `file_view`'s currently open file.
fn header_text(app: &App, file_view: &FileViewState, nvim_mode: bool) -> String {
    let display_name = app
        .graph
        .node(&app.focus)
        .map(|node| node.display_name.as_str())
        .unwrap_or("?");
    let layer_position = focused_layer_position(&app.layers, &app.focus);
    let Some(file) = file_view.current_file() else {
        return match layer_position {
            Some((layer, total)) => format!("{display_name}   layer {layer}/{total}"),
            None => display_name.to_string(),
        };
    };
    assemble_header_text(
        display_name,
        layer_position,
        file,
        file_view.file_index,
        file_view.files.len(),
        !nvim_mode,
    )
}

/// Pure text assembly given already-resolved pieces -- split out from
/// [`header_text`] so it's unit-testable without constructing a full
/// [`App`]/[`FileViewState`]. Order: display name, layer position (if
/// known), the file's repo-relative path, `(deleted)` when applicable,
/// then `(i/N)` for a multi-file node -- but only when `show_file_index`
/// (nvim mode always opens `files.first()`, so there's never a second file
/// this pane could be showing, and the indicator would just be noise).
fn assemble_header_text(
    display_name: &str,
    layer_position: Option<(usize, usize)>,
    file: &FileViewEntry,
    file_index: usize,
    file_count: usize,
    show_file_index: bool,
) -> String {
    let mut parts = vec![display_name.to_string()];
    if let Some((layer, total)) = layer_position {
        parts.push(format!("layer {layer}/{total}"));
    }
    parts.push(file.path.display().to_string());
    if file.deleted {
        parts.push("(deleted)".to_string());
    }
    if show_file_index && file_count > 1 {
        parts.push(format!("({}/{})", file_index + 1, file_count));
    }
    parts.join("   ")
}

/// `focus`'s 1-based layer index and the total layer count in `layers`
/// (e.g. `(2, 5)` renders as "layer 2/5"), or `None` if `focus` isn't in
/// any layer -- shouldn't happen while the overlay is open on a real,
/// drawn node, but this is rendering code, so it defends rather than
/// panics.
pub fn focused_layer_position(layers: &[Vec<NodeId>], focus: &NodeId) -> Option<(usize, usize)> {
    let total = layers.len();
    layers
        .iter()
        .position(|layer| layer.contains(focus))
        .map(|idx| (idx + 1, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(path: &str, deleted: bool) -> FileViewEntry {
        FileViewEntry {
            path: PathBuf::from(path),
            lines: vec![],
            changed_ranges: vec![],
            deleted,
        }
    }

    #[test]
    fn focused_layer_position_finds_the_containing_layer() {
        let layers = vec![
            vec![NodeId::from("a"), NodeId::from("b")],
            vec![NodeId::from("c")],
            vec![NodeId::from("d"), NodeId::from("e")],
        ];
        assert_eq!(
            focused_layer_position(&layers, &NodeId::from("c")),
            Some((2, 3))
        );
        assert_eq!(
            focused_layer_position(&layers, &NodeId::from("e")),
            Some((3, 3))
        );
    }

    #[test]
    fn focused_layer_position_none_when_focus_not_in_any_layer() {
        let layers = vec![vec![NodeId::from("a")]];
        assert_eq!(focused_layer_position(&layers, &NodeId::from("z")), None);
    }

    #[test]
    fn assemble_header_text_basic_fields_in_order() {
        let f = file("src/main.rs", false);
        assert_eq!(
            assemble_header_text("MyApp.Foo", Some((2, 5)), &f, 0, 1, true),
            "MyApp.Foo   layer 2/5   src/main.rs"
        );
    }

    #[test]
    fn assemble_header_text_no_layer_position() {
        let f = file("src/main.rs", false);
        assert_eq!(
            assemble_header_text("MyApp.Foo", None, &f, 0, 1, true),
            "MyApp.Foo   src/main.rs"
        );
    }

    #[test]
    fn assemble_header_text_marks_deleted_files() {
        let f = file("src/gone.rs", true);
        assert_eq!(
            assemble_header_text("MyApp.Gone", Some((1, 1)), &f, 0, 1, true),
            "MyApp.Gone   layer 1/1   src/gone.rs   (deleted)"
        );
    }

    #[test]
    fn assemble_header_text_shows_file_index_for_multi_file_nodes() {
        let f = file("src/b.rs", false);
        assert_eq!(
            assemble_header_text("MyApp.Multi", Some((1, 2)), &f, 1, 3, true),
            "MyApp.Multi   layer 1/2   src/b.rs   (2/3)"
        );
    }

    #[test]
    fn assemble_header_text_hides_file_index_for_a_single_file_node() {
        let f = file("src/only.rs", false);
        assert_eq!(
            assemble_header_text("MyApp.Only", Some((1, 1)), &f, 0, 1, true),
            "MyApp.Only   layer 1/1   src/only.rs"
        );
    }

    #[test]
    fn assemble_header_text_suppresses_file_index_in_nvim_mode_even_if_multi_file() {
        let f = file("src/b.rs", false);
        assert_eq!(
            assemble_header_text("MyApp.Multi", Some((1, 2)), &f, 1, 3, false),
            "MyApp.Multi   layer 1/2   src/b.rs"
        );
    }
}
