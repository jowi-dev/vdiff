//! The impure glue: git access, source extraction, and resolution that
//! together turn a repository into a [`crate::graph::model::ProjectGraph`].
//! Orchestration (`build_graph`) lands in a later milestone once
//! `changed_files`, `extract`, and `resolve` all exist.

pub mod changed_files;
pub mod error;
pub mod extract;
pub mod repo;
