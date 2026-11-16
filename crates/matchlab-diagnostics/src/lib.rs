//! Developer diagnostics for MatchLab. They replace the earlier Python tools:
//! a bounded HTTP auditor for labeled corpora, a process startup benchmark
//! with synthetic SQLite fixtures, and a conservative reader of explicit
//! parser-test expectations.
pub mod audit_api;
pub mod benchmark;
pub mod diff;
pub mod title_tests;
