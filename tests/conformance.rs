//! Conformance: tracked files agree with the code that owns them.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn sources() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    let mut pending = vec![root().join("src")];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                let text = fs::read_to_string(&path).unwrap();
                files.push((path, text));
            }
        }
    }
    files
}

/// Every `"ashe.worker.*"` string literal in the source.
fn signal_literals() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for (_, text) in sources() {
        for (start, _) in text.match_indices("\"ashe.worker.") {
            let rest = &text[start + 1..];
            let end = rest.find('"').unwrap();
            names.insert(rest[..end].to_string());
        }
    }
    names
}

fn catalog() -> serde_json::Value {
    let text = fs::read_to_string(root().join("observability/signal-catalog.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn strings(value: &serde_json::Value) -> impl Iterator<Item = String> + '_ {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item.as_str().unwrap().to_string())
}

#[test]
fn the_tracked_env_matches_the_input_schema() {
    let tracked = fs::read_to_string(root().join(".env")).unwrap();
    assert!(
        tracked == ashe_worker::inputs::render_env(),
        ".env is stale; regenerate it with `cargo run --bin ashe-worker-env > .env`"
    );
}

#[test]
fn the_signal_catalog_declares_exactly_the_signals_in_code() {
    let catalog = catalog();
    let mut declared = BTreeSet::new();
    for signal in ["traces", "metrics", "logs", "correlation_attributes"] {
        declared.extend(strings(&catalog["catalog"][signal]));
    }
    for signal in ["traces", "metrics", "logs"] {
        declared.extend(
            strings(&catalog["allowed_attributes"][signal])
                .filter(|name| name.starts_with("ashe.worker.")),
        );
    }
    assert_eq!(signal_literals(), declared);
}

#[test]
fn declared_attribute_values_and_error_classes_appear_in_code() {
    let catalog = catalog();
    let code: String = sources().into_iter().map(|(_, text)| text).collect();
    let mut values: Vec<String> = strings(&catalog["error_classes"]).collect();
    for (_, allowed) in catalog["attribute_values"].as_object().unwrap() {
        values.extend(strings(allowed));
    }
    for value in values {
        assert!(
            code.contains(&format!("\"{value}\"")),
            "catalog value {value} is not emitted by the code"
        );
    }
}
