#![allow(dead_code)]
//! Shared builders for the Community Learning tests.
use matchlab_community::json::{dumps, Dump, Json};
use matchlab_community::rule_intake::Catalog;
use matchlab_community::util::sha256_hex;
use std::path::PathBuf;

/// Convert a `serde_json::json!` literal.
pub fn j(value: serde_json::Value) -> Json {
    Json::from(&value)
}

/// `{**base, key: value}`.
pub fn with(base: &Json, key: &str, value: Json) -> Json {
    let mut out = base.clone();
    if let Json::Obj(map) = &mut out {
        map.insert(key.to_owned(), value);
    }
    out
}

pub fn without(base: &Json, key: &str) -> Json {
    let mut out = base.clone();
    if let Json::Obj(map) = &mut out {
        map.shift_remove(key);
    }
    out
}

pub fn sha(text: &str) -> String {
    sha256_hex(text)
}

pub fn text(value: &Json) -> String {
    dumps(value, Dump::DEFAULT)
}

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn catalog_path() -> PathBuf {
    repo_root().join("crates/matchlab-parser/src/generated/handlers.json")
}

pub fn catalog() -> Catalog {
    Catalog::load(&catalog_path())
}

pub fn handlers() -> Vec<Json> {
    catalog().handlers().expect("handler catalogue").to_vec()
}

pub fn source() -> String {
    std::fs::read_to_string(catalog_path()).unwrap()
}

pub fn yes(_: &str, _: &str) -> bool {
    true
}

pub fn no(_: &str, _: &str) -> bool {
    false
}

/// A temporary directory removed on drop.
pub struct Temp(pub PathBuf);

impl Temp {
    pub fn new() -> Temp {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "matchlab-community-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Temp(path)
    }

    pub fn file(&self, name: &str, content: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
