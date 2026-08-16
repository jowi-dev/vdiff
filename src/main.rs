use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use vdiff::cli::{self, Cli};
use vdiff::core::app::{App, Screen};
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
            eprintln!("error opening repository at {}: {err}", repo_path.display());
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
        base_override: cli.base,
    };
    let graph = match build_graph(repo.as_ref(), &opts) {
        Ok(graph) => graph,
        Err(err) => {
            eprintln!("error building graph: {err}");
            return ExitCode::FAILURE;
        }
    };

    match cli.dump {
        Some(format) => {
            println!("{}", cli::render(&graph, format));
            ExitCode::SUCCESS
        }
        None => run_gui(graph, &repo_path, cli.smoke, DiffLoader { repo, base_oid }),
    }
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
