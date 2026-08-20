//! Fullscreen editor overlay: when [`Pane::File`] is the active pane, this
//! paints on top of the already-rendered graph -- an opaque header strip,
//! then the editor content (the nvim grid, or the built-in [`file_view`] as
//! a fallback) below it. There is no separate full-viewport scrim: the
//! editor content itself paints its backgrounds translucent (see
//! [`theme::EDITOR_BG_ALPHA`]'s doc), which is what lets the graph show
//! through as a faint ambient glow. The header strip is the one exception,
//! staying fully opaque -- solid UI chrome, not part of that surface.
//!
//! This replaced a resizable 50%-width side panel. Rationale (decided with
//! the user): the graph and the editor answer different questions, and
//! splitting the viewport taxes both -- fullscreen nvim also fixes
//! plugins/UIs that feel cramped at half width. The see-through content is
//! a continuity cue ("the graph is still there, dimmed"), not an
//! information channel -- legibility of the editor's own text always wins.

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
/// caller): opaque header strip, then editor content in whatever's left.
/// `nvim` selects the content: `Some` paints the live nvim grid via
/// [`nvim_pane::show`], whose own per-cell backgrounds are translucent, so
/// nothing further is painted here; `None` falls back to the built-in
/// [`file_view::show`], which paints no background of its own, so this
/// function paints one translucent panel wash behind it first (see
/// [`theme::EDITOR_BG_ALPHA`]) to give both modes the same "graph shows
/// through" treatment. Returns the row count the built-in viewer fit into
/// its content area (for [`App::viewport_rows`]'s `Ctrl-d`/`Ctrl-u`
/// half-page math) -- `None` in nvim mode, which doesn't use it.
pub fn show(
    ui: &mut Ui,
    app: &App,
    file_view_state: &FileViewState,
    nvim: Option<&mut NvimPane>,
) -> Option<usize> {
    let screen_rect = ui.max_rect();

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
        None => {
            // nvim's own cell backgrounds are what make it translucent;
            // the built-in viewer paints no background at all, so it needs
            // this wash to match -- painted once, covering exactly the
            // content area (not the whole screen), so there's nothing here
            // for a second translucent layer to stack under.
            ui.painter()
                .rect_filled(content_rect, 0.0, theme::translucent(theme::CANVAS_BG));
            Some(file_view::show(&mut content_ui, file_view_state))
        }
    }
}

/// Assemble the header strip's text for `app`'s current layers and
/// `file_view`'s currently open file, naming `file_view.node` -- the node
/// whose file is actually shown, which is `app.focus` except right after
/// `gt` (see [`crate::core::app::Msg::GoToTest`]).
fn header_text(app: &App, file_view: &FileViewState, nvim_mode: bool) -> String {
    // The pane shows `file_view.node`'s file -- normally `app.focus`, but
    // after `gt` (see `Msg::GoToTest`) focus stays on the module while the
    // pane shows its matched test's file, so the header must name the node
    // actually on screen. `focus` is only a fallback for the (should-never-
    // happen) case where `file_view.node` isn't in the graph at all.
    let display_node = app.graph.node(&file_view.node).map(|_| &file_view.node);
    let display_node = display_node.unwrap_or(&app.focus);
    let display_name = app
        .graph
        .node(display_node)
        .map(|node| node.display_name.as_str())
        .unwrap_or("?");
    // The test node itself is never in `app.layers` (test modules are
    // pruned out of the visible layout regardless of `show_tests` when
    // matched -- see `group_matched_test_modules`), so this comes back
    // `None` after `gt` and `assemble_header_text` just omits the segment.
    let layer_position = focused_layer_position(&app.layers, display_node);
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

    /// A minimal graph with a module (`module`, display name `Foo`, laid out
    /// in `layers`) and its matched test (`module_test`, display name
    /// `FooTest`, deliberately absent from `layers` -- test nodes never
    /// appear there, see [`header_text`]'s doc), plus an `App` focused on
    /// `module` -- for [`header_text`] tests exercising the post-`gt` state
    /// where `file_view.node` differs from `focus`.
    fn app_with_test_module() -> App {
        use crate::core::app::{Pane, Screen};
        use crate::graph::model::{GitStatus, ModuleNode, ProjectGraph};
        use std::collections::HashMap;
        use std::path::PathBuf;

        let module = NodeId::from("module");
        let test = NodeId::from("module_test");
        let leaf = |id: &NodeId, name: &str| ModuleNode {
            id: id.clone(),
            display_name: name.to_string(),
            parent: None,
            children: vec![],
            status: GitStatus::Unchanged,
            files: vec![crate::graph::model::FileRef {
                path: PathBuf::from(format!("{name}.rs")),
                base_blob: Some("b".to_string()),
                head_blob: Some("h".to_string()),
            }],
        };
        let mut nodes = HashMap::new();
        nodes.insert(module.clone(), leaf(&module, "Foo"));
        nodes.insert(test.clone(), leaf(&test, "FooTest"));
        let graph = ProjectGraph {
            roots: vec![module.clone(), test.clone()],
            nodes,
            edges: vec![],
        };

        App {
            graph,
            layers: vec![vec![module.clone()]],
            rows: vec![],
            focus: module,
            screen: Screen::Graph,
            diff: None,
            picker: None,
            show_tests: false,
            file_view: None,
            pane: Pane::File,
            viewport_rows: 20,
            reviewed: std::collections::HashSet::new(),
        }
    }

    #[test]
    fn header_text_names_the_displayed_node_not_focus() {
        let mut app = app_with_test_module();
        app.file_view = Some(FileViewState::new(NodeId::from("module_test"), vec![]));
        let file_view = app.file_view.clone().unwrap();

        let text = header_text(&app, &file_view, false);

        // Names FooTest (the displayed node), not Foo (focus), and omits
        // the layer segment -- module_test isn't in `app.layers`.
        assert_eq!(text, "FooTest");
    }

    #[test]
    fn header_text_falls_back_to_focus_when_displayed_node_is_unknown() {
        let mut app = app_with_test_module();
        app.file_view = Some(FileViewState::new(NodeId::from("nonexistent"), vec![]));
        let file_view = app.file_view.clone().unwrap();

        let text = header_text(&app, &file_view, false);

        assert_eq!(text, "Foo   layer 1/1");
    }
}
