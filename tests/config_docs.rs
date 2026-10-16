//! Drift guard: every `Config` struct field is documented in
//! `docs/CONFIG.md`, and every TOML example in that doc parses with no
//! unknown keys.
//!
//! No hardware needed. String-level on purpose: this crate is binary-only
//! (`src/main.rs`), so the test reads `src/config.rs` for the `pub <field>:`
//! lines of `pub struct Config` instead of importing it.

use std::collections::HashSet;
use std::path::PathBuf;

fn repo_file(rel: &str) -> String {
    let p: PathBuf = [env!("CARGO_MANIFEST_DIR"), rel].iter().collect();
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

/// Field names of `pub struct Config` in `src/config.rs`.
fn config_fields(src: &str) -> Vec<String> {
    let start = src
        .find("pub struct Config")
        .expect("src/config.rs: `pub struct Config` missing");
    let body = &src[start..];
    let open = body.find('{').expect("Config struct has no body");
    let mut depth = 0;
    let mut end = None;
    for (i, c) in body[open..].char_indices() {
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            depth -= 1;
            if depth == 0 {
                end = Some(open + i);
                break;
            }
        }
    }
    let body = &body[open..end.expect("Config struct body never closes")];
    body.lines()
        .filter_map(|l| {
            let t = l.trim();
            t.strip_prefix("pub ")
                .and_then(|rest| rest.split(':').next())
                .map(str::trim)
                .filter(|name| {
                    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
                .map(str::to_string)
        })
        .collect()
}

/// Fenced ```toml blocks in a markdown doc.
fn toml_blocks(doc: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut cur: Option<String> = None;
    for line in doc.lines() {
        if line.trim_start().starts_with("```") {
            if let Some(block) = cur.take() {
                if !block.trim().is_empty() {
                    blocks.push(block);
                }
            } else if line.trim() == "```toml" {
                cur = Some(String::new());
            }
            continue;
        }
        if let Some(b) = cur.as_mut() {
            b.push_str(line);
            b.push('\n');
        }
    }
    assert!(!blocks.is_empty(), "docs/CONFIG.md has no ```toml blocks");
    blocks
}

#[test]
fn config_doc_covers_every_field() {
    let src = repo_file("src/config.rs");
    let doc = repo_file("docs/CONFIG.md");
    let fields = config_fields(&src);
    assert!(
        fields.len() >= 21,
        "expected >= 21 Config fields, got {fields:?}"
    );
    let missing: Vec<_> = fields
        .iter()
        .filter(|f| !doc.contains(&format!("`{f}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/CONFIG.md missing keys: {missing:?} (backtick-wrapped)"
    );
}

#[test]
fn config_doc_examples_parse_with_known_keys_only() {
    let src = repo_file("src/config.rs");
    let known: HashSet<String> = config_fields(&src).into_iter().collect();
    let doc = repo_file("docs/CONFIG.md");
    for (i, block) in toml_blocks(&doc).iter().enumerate() {
        let table: toml::Table = toml::from_str(block)
            .unwrap_or_else(|e| panic!("CONFIG.md example {i} fails to parse: {e}\n{block}"));
        assert!(
            !table.is_empty(),
            "CONFIG.md example {i} parsed to an empty table"
        );
        let unknown: Vec<_> = table.keys().filter(|k| !known.contains(*k)).collect();
        assert!(
            unknown.is_empty(),
            "CONFIG.md example {i} has unknown keys: {unknown:?}"
        );
    }
}
