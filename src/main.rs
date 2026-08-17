use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use vdiff::cli::{self, Cli};
use vdiff::core::app::{App, Screen};
use vdiff::graph::filter::focus_on_changes;
use vdiff::graph::layout::layout;
use vdiff::graph::model::{NodeId, ProjectGraph};
use vdiff::pipeline::git2_repo::Git2Repo;
use vdiff::pipeline::repo::GitRepo;
use vdiff::pipeline::{build_graph, PipelineOptions};
use vdiff::ui::eframe_app::{DiffLoader, VdiffApp};

fn main() -> ExitCode {
    let cli = Cli::parse();

    let repo_path = cli.repo.clone().unwrap_or_else(|| PathBuf::from("."));
    let repo = match Git2Repo::open(&repo_path) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    // Resolved once up front and reused for both the graph build and the
    // diff pane's later `Cmd::LoadDiff` lookups, so both agree on the same
    // base commit for the lifetime of this run.
    let base_oid = match repo.default_base_oid(cli.base.as_deref()) {
        Ok(oid) => oid,
        Err(err) => {
            eprintln!("error resolving diff base: {err}");
            return ExitCode::FAILURE;
        }
    };
    let repo: Box<dyn GitRepo> = Box::new(repo);

    let opts = PipelineOptions {
        base_override: cli.base.clone(),
    };
    let graph = match build_graph(repo.as_ref(), &opts) {
        Ok(graph) => graph,
        Err(err) => {
            eprintln!("error building graph: {err}");
            return ExitCode::FAILURE;
        }
    };

    // By default vdiff shows only the change set and the paths connecting
    // it; `--all` opts back into the raw, unfiltered graph. Filtering
    // happens once here, before both the dump and GUI paths, so layout and
    // rendering never see the unfiltered graph unless asked to.
    let graph = if cli.all {
        graph
    } else {
        focus_on_changes(&graph)
    };

    match cli.dump {
        Some(format) => dump(&graph, format, cli.include_diffs, repo.as_ref(), &base_oid),
        None => {
            if graph.nodes.is_empty() {
                let base_ref = cli.base.as_deref().unwrap_or(&base_oid);
                eprintln!("no changes vs {base_ref}");
                return ExitCode::SUCCESS;
            }
            run_gui(graph, &repo_path, cli.smoke, DiffLoader { repo, base_oid })
        }
    }
}

/// `--dump <format>`: render `graph`, computing the `--include-diffs`
/// payload first if requested. `--include-diffs` with `--dump text` is a
/// friendly CLI error (clap's `requires = "dump"` only guarantees `--dump`
/// was given at all, not which format) rather than a silent no-op.
fn dump(
    graph: &ProjectGraph,
    format: cli::DumpFormat,
    include_diffs: bool,
    repo: &dyn GitRepo,
    base_oid: &str,
) -> ExitCode {
    if include_diffs && format != cli::DumpFormat::Json {
        eprintln!("error: --include-diffs requires --dump json");
        return ExitCode::FAILURE;
    }
    let diffs = if include_diffs {
        match vdiff::pipeline::file_diff::diffs_for_graph(repo, base_oid, graph) {
            Ok(diffs) => Some(diffs),
            Err(err) => {
                eprintln!("error computing diffs: {err}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        None
    };
    println!("{}", cli::render(graph, format, diffs.as_ref()));
    ExitCode::SUCCESS
}

/// Open the eframe window on `graph`, titled after `repo_path`'s directory
/// name. `smoke` closes the window after a couple seconds instead of
/// waiting for the user, for headless-ish startup verification.
/// `diff_loader` backs `Cmd::LoadDiff` once a node's diff pane is opened.
fn run_gui(
    graph: ProjectGraph,
    repo_path: &Path,
    smoke: bool,
    diff_loader: DiffLoader,
) -> ExitCode {
    let layout_result = layout(&graph);
    let focus = graph
        .sorted_roots()
        .into_iter()
        .next()
        .unwrap_or_else(|| NodeId::from(""));
    let app = App {
        graph,
        focus,
        screen: Screen::Graph,
        diff: None,
        picker: None,
    };

    let title = format!("vdiff — {}", repo_dir_name(repo_path));
    let native_options = eframe::NativeOptions::default();
    let result = eframe::run_native(
        &title,
        native_options,
        Box::new(move |_cc| {
            Ok(Box::new(VdiffApp::new(
                app,
                layout_result,
                smoke,
                diff_loader,
            )))
        }),
    );

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error running GUI: {err}");
            ExitCode::FAILURE
        }
    }
}

/// The directory name to show in the window title: `repo_path`'s canonical
/// last path component, or `?` if that can't be determined.
fn repo_dir_name(repo_path: &Path) -> String {
    repo_path
        .canonicalize()
        .unwrap_or_else(|_| repo_path.to_path_buf())
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "?".to_string())
}
