//! The tool counts quoted in the docs must match the registry.
//!
//! CONTRIBUTING asks for `router/registry.rs`, `tool-directory.md`, DEV.md's
//! "Current Stats" and the README to move together, and notes that "those
//! three counts have drifted apart before precisely because only one of them
//! got updated". Nothing enforced it: `registry_tool_counts_match_reality`
//! only checks each toolset's `tool_count` against `tools_for()`, so a PR that
//! adds a tool and updates two of the four documents is green.
//!
//! That is not hypothetical — PRs #159 and #160 each bump `registry.rs`,
//! `tool-directory.md` and DEV.md while leaving README.md behind, and CI has
//! nothing to say about it.
//!
//! So derive the numbers from the registry and require the prose to agree.

use konnect_core::router::registry;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // crates/konnect -> crates -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root above crates/konnect")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

/// Ground truth: what the registry actually declares.
fn counts() -> (usize, usize, usize) {
    let toolsets = registry::ALL_TOOLSETS.len();
    let registered: usize = registry::ALL_TOOLSETS.iter().map(|t| t.tool_count).sum();
    let meta = konnect_core::router::meta_tools::meta_tool_descriptions().len();
    (toolsets, registered, meta)
}

/// Every number a document is required to quote, with the exact spelling to
/// look for. Kept as whole phrases rather than bare integers so a coincidental
/// "187" elsewhere in the file cannot satisfy the check.
fn required_phrases() -> Vec<(&'static str, String)> {
    let (toolsets, registered, meta) = counts();
    let total = registered + meta;
    vec![
        (
            "README.md",
            format!("**{registered} tools across {toolsets} on-demand toolsets.**"),
        ),
        (
            "DEV.md",
            format!("**{toolsets} toolsets, {registered} tools** + {meta} meta-tools"),
        ),
        (
            "DEV.md",
            format!("{total} tools ({registered} registered + {meta} meta)"),
        ),
        (
            "tool-directory.md",
            format!(
                "**{registered} registered tools** + **{meta} always-visible meta-tools** = **{total} total**"
            ),
        ),
    ]
}

#[test]
fn docs_quote_the_registry_tool_counts() {
    let mut wrong = Vec::new();
    for (file, phrase) in required_phrases() {
        if !read(file).contains(&phrase) {
            wrong.push(format!("{file} is missing: {phrase}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "tool counts have drifted from the registry.\n{}\n\n\
         Update registry.rs, tool-directory.md, DEV.md's \"Current Stats\" and \
         the README together — see CONTRIBUTING.",
        wrong.join("\n")
    );
}

/// `tool-directory.md` lists every tool in a table, so its row count is a
/// second, independent statement of the same number. A tool added to the
/// registry without a directory entry is undocumented; one listed but not
/// registered is a phantom the LLM will try to call.
#[test]
fn tool_directory_lists_every_registered_tool() {
    let directory = read("tool-directory.md");

    let mut missing = Vec::new();
    for ts in registry::ALL_TOOLSETS {
        for def in registry::tools_for(ts.name).expect("toolset resolves") {
            if !directory.contains(&format!("`{}`", def.name)) {
                missing.push(format!("{} (in {})", def.name, ts.name));
            }
        }
    }

    assert!(
        missing.is_empty(),
        "tool-directory.md does not document {} registered tool(s):\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

/// No file anywhere quotes a catalogue total that is not the current one.
///
/// The checks above name the four files CONTRIBUTING lists, which is why
/// `packaging/metadata.json` and `plugin/plugin.json` sat at "185 tools" while
/// the guarded documents said 200 — and those two are the ones users read, in
/// the PCM package description. `docs/TROUBLESHOOTING.md` said 189.
///
/// Sweeping instead of listing means a new document is covered the day it is
/// written rather than the day someone remembers to add it here. Only
/// three-digit counts are checked: a per-toolset count cannot reach 100 with
/// `MAX_TOOLS_PER_TOOLSET` at 20, so DEV.md's per-toolset tables are
/// unambiguously not catalogue totals and are left alone.
#[test]
fn no_file_quotes_a_stale_catalogue_total() {
    let (_, registered, meta) = counts();
    let total = registered + meta;

    let mut stale = Vec::new();
    for path in text_files(&repo_root()) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (lineno, line) in text.lines().enumerate() {
            for n in counts_in(line) {
                if n >= 100 && n != registered && n != total {
                    let rel = path
                        .strip_prefix(repo_root())
                        .unwrap_or(&path)
                        .display()
                        .to_string()
                        .replace('\\', "/");
                    stale.push(format!(
                        "{rel}:{}: says \"{n} tools\" — the registry has {registered} \
                         registered, {total} with meta-tools",
                        lineno + 1
                    ));
                }
            }
        }
    }

    assert!(
        stale.is_empty(),
        "a document quotes a tool count the registry does not support:\n  {}",
        stale.join("\n  ")
    );
}

/// Markdown and JSON under the repo, skipping build output and vendored trees.
fn text_files(root: &Path) -> Vec<PathBuf> {
    const SKIP: &[&str] = &["target", "node_modules", ".git", "dist", "build"];
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !SKIP.contains(&name.as_ref()) {
                    stack.push(path);
                }
            } else if matches!(
                path.extension().and_then(|e| e.to_str()),
                Some("md") | Some("json")
            ) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Numbers written immediately before the word "tools", ignoring any `~`.
fn counts_in(line: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for (at, _) in line.match_indices("tools") {
        let before = line[..at].trim_end();
        let digits: String = before
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.is_empty() {
            continue;
        }
        // Only a bare number counts: "20250610 tools" would be a version.
        if let Ok(n) = digits.chars().rev().collect::<String>().parse::<usize>() {
            out.push(n);
        }
    }
    out
}

/// No file quotes a stale **toolset** count.
///
/// The catalogue-total sweep above only looks at three-digit numbers, so a
/// two-digit toolset count slips past it by construction. That is exactly how
/// `packaging/metadata.json` and `router/meta_tools.rs` both sat at "18
/// toolsets" through a release that shipped 19 — the number users read in the
/// PCM description, and the number the router's own module doc claims.
///
/// Counted separately from tools because the plural noun differs, and because
/// a toolset count is small enough that no digit-width heuristic can separate
/// it from an unrelated number.
#[test]
fn no_file_quotes_a_stale_toolset_count() {
    let (toolsets, _, _) = counts();

    let mut stale = Vec::new();

    // The router's own doc comments make this claim too — `meta_tools.rs` said
    // "all 18 toolsets" — so sweep those sources alongside the documents.
    for path in text_files(&repo_root()).into_iter().chain(rust_sources(
        &repo_root().join("crates/konnect-core/src/router"),
    )) {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (lineno, line) in text.lines().enumerate() {
            for n in counts_before(line, "toolset") {
                if n != toolsets {
                    let rel = path
                        .strip_prefix(repo_root())
                        .unwrap_or(&path)
                        .display()
                        .to_string()
                        .replace('\\', "/");
                    stale.push(format!(
                        "{rel}:{}: says \"{n} toolset(s)\" — the registry defines {toolsets}",
                        lineno + 1
                    ));
                }
            }
        }
    }
    assert!(
        stale.is_empty(),
        "a file quotes a toolset count the registry does not support:\n  {}",
        stale.join("\n  ")
    );
}

/// Every `### \`toolset\` · N tools` heading in `tool-directory.md` matches that
/// toolset's `tool_count`.
///
/// `sch_components` shipped a release reading "19 tools" over a table of 20
/// rows. Both the file's own overview (202) and the registry (20) disagreed
/// with it, and nothing noticed: the catalogue sweep ignores two-digit numbers,
/// and `tool_directory_lists_every_registered_tool` only checks that each tool
/// appears *somewhere* in the file, not that the section headings add up.
#[test]
fn tool_directory_section_headings_match_the_registry() {
    let directory = read("tool-directory.md");
    let mut wrong = Vec::new();
    let mut seen = 0usize;

    for (lineno, line) in directory.lines().enumerate() {
        let Some(rest) = line.strip_prefix("### `") else {
            continue;
        };
        let Some((name, tail)) = rest.split_once('`') else {
            continue;
        };
        let Some(meta) = registry::ALL_TOOLSETS.iter().find(|ts| ts.name == name) else {
            continue; // a heading that is not a toolset
        };
        seen += 1;
        let claimed: Option<usize> = tail
            .split_whitespace()
            .find_map(|word| word.parse::<usize>().ok());
        match claimed {
            Some(n) if n == meta.tool_count => {}
            Some(n) => wrong.push(format!(
                "tool-directory.md:{}: `{name}` heading says {n} tools, registry says {}",
                lineno + 1,
                meta.tool_count
            )),
            None => wrong.push(format!(
                "tool-directory.md:{}: `{name}` heading states no tool count",
                lineno + 1
            )),
        }
    }

    assert_eq!(
        seen,
        registry::ALL_TOOLSETS.len(),
        "tool-directory.md should have one section heading per toolset"
    );
    assert!(
        wrong.is_empty(),
        "tool-directory.md section headings disagree with the registry:\n  {}",
        wrong.join("\n  ")
    );
}

/// `.rs` files under `dir`, for the doc-comment sweeps.
fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            files.push(path);
        }
    }
    files.sort();
    files
}

/// Numbers written immediately before `noun` (matching its plural too).
fn counts_before(line: &str, noun: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for (at, _) in line.match_indices(noun) {
        let before = line[..at].trim_end();
        let digits: String = before
            .chars()
            .rev()
            .take_while(char::is_ascii_digit)
            .collect();
        if digits.is_empty() {
            continue;
        }
        if let Ok(n) = digits.chars().rev().collect::<String>().parse::<usize>() {
            out.push(n);
        }
    }
    out
}

/// The meta-tool count is quoted in prose too, and it moves far less often —
/// which is exactly why a change to it is easy to forget. PR #176 proposes
/// taking it from 6 to 7.
#[test]
fn docs_quote_the_meta_tool_count() {
    let (_, _, meta) = counts();
    let dev = read("DEV.md");
    assert!(
        dev.contains(&format!("{meta} meta-tools")),
        "DEV.md must state \"{meta} meta-tools\" — the registry defines {meta}"
    );
}
