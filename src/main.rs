use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use vdiff::cli::{self, Cli};
use vdiff::pipeline::git2_repo::Git2Repo;
use vdiff::pipeline::{build_graph, PipelineOptions};

fn main() -> ExitCode {
    let cli = Cli::parse();

    let Some(format) = cli.dump else {
        eprintln!("GUI not yet built; pass --dump text|json");
        return ExitCode::from(2);
    };

    let repo_path = cli.repo.unwrap_or_else(|| PathBuf::from("."));
    let repo = match Git2Repo::open(&repo_path) {
        Ok(repo) => repo,
        Err(err) => {
            eprintln!("error opening repository at {}: {err}", repo_path.display());
            return ExitCode::FAILURE;
        }
    };

    let opts = PipelineOptions {
        base_override: cli.base,
    };
    let graph = match build_graph(&repo, &opts) {
        Ok(graph) => graph,
        Err(err) => {
            eprintln!("error building graph: {err}");
            return ExitCode::FAILURE;
        }
    };

    println!("{}", cli::render(&graph, format));
    ExitCode::SUCCESS
}
