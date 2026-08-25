//! Headless TUI snapshot harness (uncommitted dev scratch): builds the
//! graph from a real repo exactly like `launch_tui`, renders one frame of
//! a chosen view to a ratatui TestBackend, and prints the buffer as plain
//! text. Usage:
//!
//! ```sh
//! cargo run --example tui_snapshot --features tui -- \
//!     <repo_path> [base_ref] [rail|canvas] [width] [height] [zo_count]
//! ```

use std::collections::{HashMap, HashSet};

use ratatui::{backend::TestBackend, Terminal};
use vdiff::core::app::{update, App, Msg, Pane, Screen};
use vdiff::graph::filter::focus_on_changes;
use vdiff::graph::layout::{layout, rows_with_x_centers};
use vdiff::graph::model::NodeId;
use vdiff::graph::test_modules::hide_test_modules;
use vdiff::pipeline::git2_repo::Git2Repo;
use vdiff::pipeline::{build_graph, PipelineOptions};
use vdiff::tui::render::{draw, ScrollOffsets};
use vdiff::tui::{seed_fold_collapsed_if_dense, ViewMode};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let repo_path = args.get(1).expect("repo path required");
    let base = args.get(2).cloned();
    let mode = match args.get(3).map(String::as_str) {
        Some("rail") => ViewMode::Rail,
        Some("canvas") => ViewMode::Canvas,
        _ => ViewMode::Plane,
    };
    let width: u16 = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(190);
    let height: u16 = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(50);
    let zo_count: usize = args.get(6).and_then(|s| s.parse().ok()).unwrap_or(0);

    let repo = Git2Repo::open(std::path::Path::new(repo_path)).expect("open repo");
    let opts = PipelineOptions {
        base_override: base,
    };
    let graph = build_graph(&repo, &opts).expect("build graph");
    let graph = focus_on_changes(&graph);
    let (visible, _) = hide_test_modules(&graph);
    let layout_result = layout(&visible);
    let focus = layout_result
        .layers
        .first()
        .and_then(|layer| layer.first())
        .cloned()
        .unwrap_or_else(|| NodeId::from(""));
    let rows = rows_with_x_centers(&layout_result);

    let mut app = App {
        graph,
        layers: layout_result.layers.clone(),
        rows,
        focus,
        screen: Screen::Graph,
        diff: None,
        picker: None,
        show_tests: false,
        file_view: None,
        pane: Pane::Graph,
        viewport_rows: height as usize,
        reviewed: HashSet::new(),
        findings: HashMap::new(),
        comments: HashMap::new(),
        fold_collapsed: HashSet::new(),
    };
    let seeded = seed_fold_collapsed_if_dense(&mut app);
    eprintln!(
        "fold seeded: {seeded}; visible nodes: {}; edges: {}; fold set: {:?}",
        app.layers.iter().map(Vec::len).sum::<usize>(),
        app.graph.edges.len(),
        app.fold_collapsed
    );

    for _ in 0..zo_count {
        let (next, _) = update(app, Msg::ExpandFocusedNamespace);
        app = next;
        eprintln!(
            "after zo: focus={:?} folds={:?}",
            app.focus, app.fold_collapsed
        );
    }

    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
    terminal
        .draw(|frame| draw(frame, &app, None, ScrollOffsets::default(), mode, None))
        .expect("draw");
    let buffer = terminal.backend().buffer();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
}
