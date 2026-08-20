use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;

use vdiff::cli::{self, Cli};
use vdiff::core::app::{App, Pane, Screen};
use vdiff::graph::filter::focus_on_changes;
use vdiff::graph::layout::layout;
use vdiff::graph::model::{NodeId, ProjectGraph};
use vdiff::graph::test_modules::hide_test_modules;
use vdiff::nvim::session::nvim_available;
use vdiff::pipeline::git2_repo::Git2Repo;
use vdiff::pipeline::pr::PrCheckout;
use vdiff::pipeline::repo::GitRepo;
use vdiff::pipeline::{build_graph, PipelineOptions};
use vdiff::review::comments::{map_comments, Comment};
use vdiff::review::findings::{map_findings, parse_findings, Finding};
use vdiff::ui::eframe_app::{DiffLoader, NvimConfig, ReviewConfig, VdiffApp};
use vdiff::ui::nvim_pane::NvimPane;

/// Everything [`run_gui`] needs to seed and later persist the
/// review-completion store (issue #4), bundled so `run_gui` stays under
/// clippy's argument-count limit: the repository's actual git directory
/// (see [`vdiff::pipeline::repo::GitRepo::git_dir`]) and current branch
/// name (see [`GitRepo::current_branch`]). Also carries `--findings
/// <path>`'s already-loaded-and-mapped result (see [`load_findings`]) --
/// unrelated to review-completion, but seeded once at startup exactly the
/// same way, so it rides along in this bundle rather than pushing
/// `run_gui` over the argument-count limit on its own.
struct ReviewSetup {
    git_dir: PathBuf,
    branch: String,
    findings: Findings,
    /// The local review-comment store (see [`vdiff::review::comments`]),
    /// already mapped onto `graph`'s node ids the same way `findings` is --
    /// see [`load_comments`]. Empty when the store doesn't exist yet or has
    /// nothing in it; never fatal, unlike a broken `--findings` file, since
    /// comments are vdiff's own bookkeeping rather than an agent-contract
    /// artifact.
    comments: Comments,
}

/// `--findings <path>`'s already-loaded-and-mapped result (see
/// [`load_findings`]), threaded into `App::findings` as-is -- `core` never
/// re-derives this, it's pure lookup data seeded once at startup.
type Findings = std::collections::HashMap<NodeId, Vec<Finding>>;

/// `<git_dir>/vdiff/comments.json`'s already-loaded-and-mapped result (see
/// [`load_comments`]), threaded into `App::comments` as-is.
type Comments = std::collections::HashMap<NodeId, Vec<Comment>>;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let requested_repo_path = cli.repo.clone().unwrap_or_else(|| PathBuf::from("."));

    // `--pr <n>` resolves against the caller's actual checkout (that's
    // where `gh`/`git fetch` run), then substitutes a temporary worktree
    // and the PR's base branch for the rest of startup -- see
    // `vdiff::pipeline::pr`'s module doc for the `--base` interaction and
    // cleanup semantics.
    let pr_checkout = match cli.pr {
        Some(pr_number) => match PrCheckout::create(&requested_repo_path, pr_number) {
            Ok(checkout) => Some(checkout),
            Err(err) => {
                eprintln!("error resolving --pr {pr_number}: {err}");
                return ExitCode::FAILURE;
            }
        },
        None => None,
    };

    let repo_path = pr_checkout
        .as_ref()
        .map(|checkout| checkout.worktree_path().to_path_buf())
        .unwrap_or(requested_repo_path);
    // `--base` wins over `--pr`'s own base branch when both are given.
    let base_override = cli.base.clone().or_else(|| {
        pr_checkout
            .as_ref()
            .map(|checkout| checkout.base_ref().to_string())
    });

    let exit_code = run(&cli, &repo_path, base_override);

    if let Some(checkout) = &pr_checkout {
        checkout.cleanup_best_effort();
    }

    exit_code
}

/// The bulk of vdiff's startup logic: open the repo at `repo_path`, resolve
/// the diff base (`base_override`, defaulting per [`GitRepo::default_base_oid`]),
/// build the graph, and either dump or launch the GUI. Parameterized on
/// `repo_path`/`base_override` rather than reading them off `cli` directly
/// so `main`'s `--pr` handling can substitute a temporary worktree and the
/// PR's base branch transparently.
fn run(cli: &Cli, repo_path: &Path, base_override: Option<String>) -> ExitCode {
    let repo = match Git2Repo::open(repo_path) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    if cli.export_comments {
        return export_comments(repo_path);
    }

    // Resolved once up front and reused for both the graph build and the
    // diff pane's later `Cmd::LoadDiff` lookups, so both agree on the same
    // base commit for the lifetime of this run.
    let base_oid = match repo.default_base_oid(base_override.as_deref()) {
        Ok(oid) => oid,
        Err(err) => {
            eprintln!("error resolving diff base: {err}");
            return ExitCode::FAILURE;
        }
    };
    let repo: Box<dyn GitRepo> = Box::new(repo);

    let opts = PipelineOptions {
        base_override: base_override.clone(),
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
                let base_ref = base_override.as_deref().unwrap_or(&base_oid);
                eprintln!("no changes vs {base_ref}");
                return ExitCode::SUCCESS;
            }
            let findings = match load_findings(cli.findings.as_deref(), &graph) {
                Ok(findings) => findings,
                Err(message) => {
                    eprintln!("error: {message}");
                    return ExitCode::FAILURE;
                }
            };
            let git_dir = repo.git_dir();
            let comments = load_comments(&git_dir, &graph);
            let review_setup = ReviewSetup {
                git_dir,
                branch: repo.current_branch(),
                findings,
                comments,
            };
            run_gui(
                graph,
                repo_path,
                cli.smoke,
                cli.nvim,
                cli.nvim_cmd.clone(),
                DiffLoader { repo, base_oid },
                review_setup,
            )
        }
    }
}

/// `--findings <path>`: read the file, parse it (see
/// [`vdiff::review::findings::parse_findings`]), and map it onto `graph`
/// (see [`map_findings`]) -- `None` (the flag wasn't given) short-circuits
/// to an empty map with no IO at all. A read or parse/validation error is
/// fatal, per `--findings`'s doc: a `findings.json` a review agent produced
/// is an agent-contract artifact, and silently limping past a broken one
/// would hide a broken pipeline rather than surface it. An unmatched
/// finding (unknown `node_id`, or a `path` matching nothing in `graph`)
/// is not fatal -- see [`map_findings`]'s doc -- but is worth a one-line
/// warning per entry so a reviewer running with a narrower graph than the
/// one the agent saw (the default `focus_on_changes` view, say) knows why
/// a finding they expected isn't showing up.
fn load_findings(path: Option<&Path>, graph: &ProjectGraph) -> Result<Findings, String> {
    let Some(path) = path else {
        return Ok(Findings::new());
    };
    let contents = std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let findings = parse_findings(&contents).map_err(|err| format!("{}: {err}", path.display()))?;
    let mapped = map_findings(graph, &findings);
    for index in &mapped.unmatched {
        eprintln!(
            "warning: finding at index {index} in {} matched no node in the current graph (unknown node_id, or a path not backing any drawn node)",
            path.display()
        );
    }
    Ok(mapped.by_node)
}

/// Load `<git_dir>/vdiff/comments.json` (see [`vdiff::review::store::load`])
/// and map it onto `graph` (see [`map_comments`]) -- a missing store (no
/// comments captured yet) or a store with nothing in it both come back as
/// an empty map, no badges drawn, same as `--findings` never having been
/// given. Unlike [`load_findings`], a read/parse failure here isn't fatal:
/// `comments.json` is vdiff's own bookkeeping (written by `vdiff.nvim`,
/// never hand-authored the way a `--findings` payload is), so a corrupt
/// file degrades to "no comment badges" with a stderr warning rather than
/// refusing to start the GUI over it.
fn load_comments(git_dir: &Path, graph: &ProjectGraph) -> Comments {
    match vdiff::review::store::load(git_dir) {
        Ok(comments) => map_comments(graph, &comments),
        Err(err) => {
            eprintln!(
                "warning: failed to load {}: {err}",
                vdiff::review::store::comments_path(git_dir).display()
            );
            Comments::new()
        }
    }
}

/// `--export-comments`: print every captured review comment (see
/// [`vdiff::review::comments`]) as markdown to stdout and exit -- headless,
/// like `--dump`, and doesn't need a graph build at all (comments are keyed
/// by path/line, not by node). Re-discovers the repository via `git2`
/// directly to get its actual git directory (`repo.path()` -- see
/// [`vdiff::pipeline::repo::GitRepo::git_dir`]'s doc for why this can't be
/// `<worktree>/.git` joined by hand) and current branch name for the
/// markdown header; the workdir is only used for the header's repo-name
/// display, with a friendly fallback for a bare repository (no workdir) --
/// unlike the GUI/`--dump` paths, comments don't otherwise need one.
fn export_comments(repo_path: &Path) -> ExitCode {
    let repo = match git2::Repository::discover(repo_path) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let branch = repo
        .head()
        .ok()
        .and_then(|head| head.shorthand().ok().map(str::to_string))
        .unwrap_or_else(|| "HEAD".to_string());
    let repo_name = repo
        .workdir()
        .map(repo_dir_name)
        .unwrap_or_else(|| "(bare repository)".to_string());
    let git_dir = repo.path();

    let comments = match vdiff::review::store::load(git_dir) {
        Ok(comments) => comments,
        Err(err) => {
            eprintln!(
                "error loading {}: {err}",
                vdiff::review::store::comments_path(git_dir).display()
            );
            return ExitCode::FAILURE;
        }
    };

    print!(
        "{}",
        vdiff::review::comments::render_markdown(&comments, &repo_name, &branch)
    );
    ExitCode::SUCCESS
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
/// `want_nvim` is the `nvim` field (on by default; `--no-nvim` clears it):
/// if set but no `nvim` binary is on `PATH`, prints a warning and falls
/// back to the built-in file viewer rather than failing to start.
/// `nvim_cmd` is `--nvim-cmd`'s Ex commands, run after every attach/
/// respawn; ignored (silently) when nvim mode isn't active. `git_dir`/
/// `branch` seed and later persist the review-completion store (issue #4):
/// `<git_dir>/vdiff/review-state.json` is loaded here, `branch`'s entry run
/// through `core::review::invalidate` against `graph`, and the result
/// seeds `App::reviewed` -- the same `git_dir`/branch then back `VdiffApp`'s
/// own save-on-toggle via `ReviewConfig`.
fn run_gui(
    graph: ProjectGraph,
    repo_path: &Path,
    smoke: bool,
    want_nvim: bool,
    nvim_cmd: Vec<String>,
    diff_loader: DiffLoader,
    review_setup: ReviewSetup,
) -> ExitCode {
    let ReviewSetup {
        git_dir,
        branch,
        findings,
        comments,
    } = review_setup;
    if want_nvim && !nvim_available() {
        eprintln!("warning: nvim mode is on by default but no `nvim` binary was found on PATH; falling back to the built-in file viewer");
    }
    let want_nvim = want_nvim && nvim_available();
    let repo_root = repo_path.to_path_buf();
    // `show_tests` defaults to false, so the layout/layers vdiff opens on
    // must be built from the test-hidden graph, not the raw (possibly
    // test-heavy) one -- see `App::visible_graph`.
    let visible = hide_test_modules(&graph).0;
    let layout_result = layout(&visible);
    // The first node of the first layer, not `graph.sorted_roots()[0]` --
    // roots can be synthetic namespace containers, which are never drawn or
    // focusable (see `graph::layers`).
    let focus = layout_result
        .layers
        .first()
        .and_then(|layer| layer.first())
        .cloned()
        .unwrap_or_else(|| NodeId::from(""));
    let review_store = vdiff::review::store::load_review_state(&git_dir);
    let reviewed =
        vdiff::review::review_state::seed_reviewed(&review_store.branch(&branch), &graph);

    let app = App {
        graph,
        layers: layout_result.layers.clone(),
        rows: vdiff::graph::layout::rows_with_x_centers(&layout_result),
        focus,
        screen: Screen::Graph,
        diff: None,
        picker: None,
        show_tests: false,
        file_view: None,
        pane: Pane::Graph,
        viewport_rows: 1,
        reviewed,
        findings,
        comments,
    };

    let title = format!("vdiff — {}", repo_dir_name(repo_path));
    // Start maximized rather than macOS native fullscreen: fullscreen opens
    // a separate Space, which is more disorienting than helpful for a dev
    // tool the user switches in and out of constantly. Maximized still
    // gives this data-dense graph the whole screen without that jump.
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_maximized(true),
        ..Default::default()
    };
    let result = eframe::run_native(
        &title,
        native_options,
        Box::new(move |cc| {
            let nvim = if want_nvim {
                // Arbitrary starting size -- `nvim_pane::show` sends a real
                // `Resize` the moment it measures the actual panel size on
                // the first frame (see `NvimPane::maybe_resize`).
                match NvimPane::spawn(&repo_root, 80, 24, cc.egui_ctx.clone()) {
                    Ok(pane) => {
                        pane.register_vdiff_commands();
                        run_nvim_init_commands(&pane, &nvim_cmd);
                        Some(pane)
                    }
                    Err(err) => {
                        eprintln!("warning: failed to spawn nvim: {err}");
                        None
                    }
                }
            } else {
                None
            };
            Ok(Box::new(VdiffApp::new(
                app,
                layout_result,
                smoke,
                diff_loader,
                NvimConfig {
                    pane: nvim,
                    cwd: repo_root.clone(),
                    init_cmds: nvim_cmd.clone(),
                    egui_ctx: cc.egui_ctx.clone(),
                },
                ReviewConfig {
                    store: review_store,
                    branch,
                },
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

/// Run each `--nvim-cmd` in order via [`NvimPane::run_init_command`],
/// logging (never failing the run over) any that error out or time out.
/// `VdiffApp::respawn_nvim` re-runs the same commands after a dead session
/// is replaced (see `NvimConfig::init_cmds`, which carries `commands`
/// forward for that).
fn run_nvim_init_commands(pane: &NvimPane, commands: &[String]) {
    for command in commands {
        if let Err(message) = pane.run_init_command(command) {
            eprintln!("warning: {message}");
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
