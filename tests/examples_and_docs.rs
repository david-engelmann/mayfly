//! Integration pins for obligation MF-16 covering docs/GUIDE.md,
//! verified README.md installation commands, examples/quickstart.rs,
//! and examples/sandbox_walkthrough.py.

#[path = "../examples/quickstart.rs"]
mod quickstart;

use std::path::Path;
use std::process::Command;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn find_python() -> &'static str {
    for candidate in ["python3", "python"] {
        if Command::new(candidate)
            .args(["-c", "import sys"])
            .status()
            .is_ok_and(|status| status.success())
        {
            return candidate;
        }
    }
    panic!("neither python3 nor python is runnable on PATH");
}

fn strip_markdown_link_targets(line: &str) -> String {
    let mut current = line.to_string();
    loop {
        let mut out = String::with_capacity(current.len());
        let bytes = current.as_bytes();
        let mut i = 0;
        let mut changed = false;
        while i < bytes.len() {
            let bang = bytes[i] == b'!' && i + 1 < bytes.len() && bytes[i + 1] == b'[';
            let open = if bang { i + 1 } else { i };
            if bytes[open] == b'[' {
                if let Some(rel_close) = current[open + 1..].find(']') {
                    let close = open + 1 + rel_close;
                    let label = &current[open + 1..close];
                    if !label.contains('[')
                        && !label.contains(']')
                        && close + 1 < bytes.len()
                        && bytes[close + 1] == b'('
                    {
                        if let Some(rel_end) = current[close + 2..].find(')') {
                            out.push_str(label);
                            i = close + 2 + rel_end + 1;
                            changed = true;
                            continue;
                        }
                    }
                }
            }
            let ch = current[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
        if !changed {
            return out;
        }
        current = out;
    }
}

fn strip_inline_code_spans(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut in_code = false;
    for ch in line.chars() {
        if ch == '`' {
            in_code = !in_code;
        } else if !in_code {
            out.push(ch);
        }
    }
    out
}

fn is_table_delimiter_row(trimmed: &str) -> bool {
    let inner = trimmed.trim_matches('|').trim();
    !inner.is_empty()
        && inner.split('|').all(|cell| {
            let c = cell.trim();
            c.contains('-') && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ' | '\t'))
        })
}

fn check_prose_violations(text: &str) -> Vec<String> {
    let mut violations = Vec::new();
    if text.contains('\u{2014}') {
        violations.push("em dash U+2014".to_string());
    }
    if text.contains('\u{2013}') {
        violations.push("en dash U+2013".to_string());
    }
    let mut in_fence = false;
    let mut fence_marker = String::new();
    for (idx, raw) in text.lines().enumerate() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            let ch = trimmed.chars().next().unwrap();
            let count = trimmed.chars().take_while(|c| *c == ch).count();
            let marker: String = std::iter::repeat_n(ch, count).collect();
            if !in_fence {
                in_fence = true;
                fence_marker = marker;
            } else if trimmed.starts_with(&fence_marker) {
                in_fence = false;
            }
            continue;
        }
        if in_fence || trimmed.starts_with("<!--") {
            continue;
        }
        let cleaned = if trimmed.starts_with('|') {
            if is_table_delimiter_row(trimmed) {
                continue;
            }
            strip_markdown_link_targets(&strip_inline_code_spans(raw))
        } else {
            strip_markdown_link_targets(raw)
        };
        if cleaned.contains(':') {
            violations.push(format!("line {}: colon in prose: {raw}", idx + 1));
        }
        if cleaned.contains(';') {
            violations.push(format!("line {}: semicolon in prose: {raw}", idx + 1));
        }
    }
    violations
}

fn assert_clean_prose(path: &Path, text: &str) {
    let violations = check_prose_violations(text);
    assert!(
        violations.is_empty(),
        "{} failed prose check: {violations:?}",
        path.display()
    );
}

fn assert_sanitized(path: &Path, text: &str) {
    for forbidden in ["/Users/", "/home/david", "@gmail.com"] {
        assert!(
            !text.contains(forbidden),
            "{} contains unsanitized marker {forbidden:?}",
            path.display()
        );
    }
}

#[test]
fn prose_scanner_rejects_colons_semicolons_and_table_prose_punctuation() {
    assert!(!check_prose_violations("a\u{2014}b").is_empty());
    assert!(!check_prose_violations("a\u{2013}b").is_empty());
    assert!(!check_prose_violations("Plain sentence: bad.").is_empty());
    assert!(!check_prose_violations("Plain sentence; bad.").is_empty());
    assert!(!check_prose_violations("| col | cell: bad |\n").is_empty());
    assert!(!check_prose_violations("| col | cell; bad |\n").is_empty());
    assert!(
        check_prose_violations(
            "Clean [guide](https://example.test/a:b).\n| col | desc |\n| --- | :---: |\n| `a:b` | clean cell |\n"
        )
        .is_empty()
    );
}

#[test]
fn operator_guide_and_contract_amendment_satisfy_mf16() {
    let contract_path = repo_root().join("CONTRACT.md");
    let contract = std::fs::read_to_string(&contract_path).expect("CONTRACT.md exists");
    let (_, mf16_section) = contract
        .split_once(
            "## Operator guide, runnable quickstarters, and verified install paths amendment",
        )
        .expect("MF-16 amendment section present");
    assert_clean_prose(&contract_path, mf16_section);
    assert_sanitized(&contract_path, mf16_section);

    let guide_path = repo_root().join("docs/GUIDE.md");
    let guide = std::fs::read_to_string(&guide_path).expect("docs/GUIDE.md exists");
    assert_clean_prose(&guide_path, &guide);
    assert_sanitized(&guide_path, &guide);
    assert!(!guide.contains("cargo install mayfly"));

    for needle in [
        "NamespaceBackend",
        "/work",
        "/tmp",
        "01777",
        "--net",
        "--ro-bind",
        "symlink_metadata",
        "--allow-env",
        "MAYFLY_SANDBOX=namespace",
        "PYTHONCOERCECLOCALE=0",
        "--timeout",
        "--max-cpu",
        "--max-memory",
        "16777216",
        "18446744073709551614",
        "janitor",
        "orphan sweep",
        "MF-12",
        "cargo build --bin mayfly",
        "examples/quickstart.rs",
        "examples/sandbox_walkthrough.py",
        "portcullis",
        "deadbolt",
        "arvin",
    ] {
        assert!(guide.contains(needle), "docs/GUIDE.md missing {needle:?}");
    }
}

#[test]
fn readme_install_avoids_third_party_bare_crate_and_links_guides() {
    let readme_path = repo_root().join("README.md");
    let readme = std::fs::read_to_string(&readme_path).expect("README.md exists");
    assert_clean_prose(&readme_path, &readme);
    assert_sanitized(&readme_path, &readme);

    for required in [
        "cargo install --git https://github.com/david-engelmann/mayfly.git --tag v0.2.0 --bin mayfly",
        "https://github.com/david-engelmann/mayfly/releases/tag/v0.2.0",
        "cargo install --path . --bin mayfly",
        "[docs/GUIDE.md](docs/GUIDE.md)",
        "[examples/quickstart.rs](examples/quickstart.rs)",
        "[examples/sandbox_walkthrough.py](examples/sandbox_walkthrough.py)",
    ] {
        assert!(readme.contains(required), "README.md missing {required:?}");
    }
    assert!(
        !readme.contains("cargo install mayfly"),
        "README.md must not instruct bare cargo install mayfly"
    );
}

#[test]
fn rust_quickstart_example_runs_end_to_end() {
    assert_sanitized(
        &repo_root().join("examples/quickstart.rs"),
        &std::fs::read_to_string(repo_root().join("examples/quickstart.rs")).unwrap(),
    );
    mayfly::set_helper_exe(Path::new(env!("CARGO_BIN_EXE_mayfly")));
    quickstart::run_quickstart().expect("examples/quickstart.rs runs end to end");
}

#[test]
fn python_sandbox_walkthrough_runs_end_to_end() {
    let script = repo_root().join("examples/sandbox_walkthrough.py");
    assert_sanitized(&script, &std::fs::read_to_string(&script).unwrap());

    let output = Command::new(find_python())
        .arg(&script)
        .arg("--mayfly-bin")
        .arg(env!("CARGO_BIN_EXE_mayfly"))
        .output()
        .expect("spawn examples/sandbox_walkthrough.py");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "sandbox_walkthrough.py failed ({:?}): stdout={stdout} stderr={stderr}",
        output.status.code()
    );
    for needle in [
        "1. mayfly 0.2.0",
        "2. --timeout 0 refused at CLI parse (exit 2)",
        "3. missing --ro-bind source refused with harness_error (exit 125)",
    ] {
        assert!(
            stdout.contains(needle),
            "sandbox_walkthrough.py stdout missing {needle:?}: {stdout}"
        );
    }

    #[cfg(target_os = "linux")]
    assert!(
        stdout.contains("4. Linux sandbox task succeeded and incinerated directory")
            || stdout.contains("4. host restricted unprivileged user namespaces:"),
        "expected verified Linux step 4 output, got: {stdout}"
    );

    #[cfg(not(target_os = "linux"))]
    assert!(
        stdout.contains("4. non-Linux host detected; pre-prepare CLI checks verified"),
        "expected non-Linux step 4 output, got: {stdout}"
    );
}
