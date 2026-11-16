//! Trusted GitHub Actions tooling for MatchLab Community Learning.
//!
//! Untrusted GitHub Issue comments are data, never code: every proposal is
//! schema-validated, fingerprinted and replayed with the current Rust parser
//! before a human-review PR can be staged. This crate replaces the earlier
//! Python scripts and keeps their file formats byte-compatible.
pub mod json;

pub mod apply;
pub mod backlog;
pub mod cleanup;
pub mod cli;
pub mod decisions;
pub mod evidence;
pub mod github;
pub mod issue_intake;
pub mod learning;
pub mod regex_plan;
pub mod review;
pub mod review_diff;
pub mod rule_intake;
pub mod state;
pub mod util;
pub mod workflow;
