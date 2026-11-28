//! Integration pins for obligations MF-16 and MF-17 covering docs/GUIDE.md,
//! SVG hero and architecture assets, CONTRIBUTING.md, GitHub issue forms,
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
    assert!(
        check_prose_violations("Intro line.\n```sh\nrun it: now; done\n```\nAfter line.\n")
            .is_empty()
    );
    assert!(!check_prose_violations("Intro line.\n```sh\nrun it\n```\nAfter: bad.\n").is_empty());
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
    let source = std::fs::read_to_string(repo_root().join("examples/quickstart.rs")).unwrap();
    for required in [
        "1. resolved backend=",
        "2. pre-prepare timeout=0 refused cleanly before directory creation",
        "3. pre-prepare missing --ro-bind source refused cleanly",
        "4. Linux namespace sandbox ran and incinerated cleanly",
        "4. host restricted unprivileged user namespaces",
        "4. live namespace spawn requires Linux (spec and pre-prepare checks passed)",
    ] {
        assert!(
            source.contains(required),
            "examples/quickstart.rs missing {required:?}"
        );
    }
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

#[test]
fn visual_assets_contributing_and_issue_forms_satisfy_mf17() {
    let contract_path = repo_root().join("CONTRACT.md");
    let contract = std::fs::read_to_string(&contract_path).expect("CONTRACT.md exists");
    let (_, mf17_section) = contract
        .split_once("## Visual architecture, contributor guide, and issue forms amendment")
        .expect("MF-17 amendment section present");
    assert_clean_prose(&contract_path, mf17_section);
    assert_sanitized(&contract_path, mf17_section);
    assert!(mf17_section.contains("Obligation MF-17"));

    for rel in ["docs/assets/hero.svg", "docs/assets/architecture.svg"] {
        let svg_path = repo_root().join(rel);
        let svg = std::fs::read_to_string(&svg_path).unwrap_or_else(|_| panic!("{rel} exists"));
        assert_sanitized(&svg_path, &svg);
        assert!(!svg.contains('\u{2014}') && !svg.contains('\u{2013}'));
        assert!(svg.contains("<svg ") && svg.contains("viewBox=") && svg.contains("</svg>"));
        let status = Command::new(find_python())
            .args([
                "-c",
                "import sys, xml.etree.ElementTree as ET; root = ET.parse(sys.argv[1]).getroot(); assert root.tag in ('{http://www.w3.org/2000/svg}svg', 'svg') and 'viewBox' in root.attrib",
            ])
            .arg(&svg_path)
            .status()
            .expect("parse SVG with ElementTree");
        assert!(status.success(), "{rel} failed strict XML SVG parse");
    }
    let reject_notsvg = Command::new(find_python())
        .args([
            "-c",
            "import xml.etree.ElementTree as ET; root = ET.fromstring('<notsvg viewBox=\"0 0 1 1\"><svg/></notsvg>'); assert root.tag in ('{http://www.w3.org/2000/svg}svg', 'svg')",
        ])
        .output()
        .expect("negative SVG root check");
    assert!(!reject_notsvg.status.success());

    let hero_svg =
        std::fs::read_to_string(repo_root().join("docs/assets/hero.svg")).expect("hero.svg");
    assert!(hero_svg.contains("51a2b1241885") && hero_svg.contains("104.36"));
    let arch_svg = std::fs::read_to_string(repo_root().join("docs/assets/architecture.svg"))
        .expect("architecture.svg");
    assert!(arch_svg.contains("Watches runner pipe, checks marker"));
    assert!(!arch_svg.contains("status pipe"));

    let readme = std::fs::read_to_string(repo_root().join("README.md")).expect("README.md");
    for needle in [
        "![mayfly overview](docs/assets/hero.svg)",
        "![mayfly architecture](docs/assets/architecture.svg)",
        "## Architecture",
        "ci.yml/badge.svg",
        "portability.yml/badge.svg",
        "img.shields.io/github/v/release/david-engelmann/mayfly",
        "linux-x86__64%20%7C%20aarch64",
        "license-MIT-blue.svg",
        "[Install](#install)",
        "[Quickstart](#quickstart)",
        "[Architecture](#architecture)",
        "[Usage](#usage)",
        "[Measurements](#measurements)",
        "[Guides and examples](#guides-and-examples)",
        "[Operator Guide](docs/GUIDE.md)",
        "[Contributing](CONTRIBUTING.md)",
        "[Contract](CONTRACT.md)",
        "[CONTRIBUTING.md](CONTRIBUTING.md)",
        "[SECURITY.md](SECURITY.md)",
    ] {
        assert!(readme.contains(needle), "README.md missing {needle:?}");
    }
    for forbidden in [
        "crates.io/crates/mayfly",
        "shields.io/crates/",
        "docs.rs/mayfly",
    ] {
        assert!(
            !readme.contains(forbidden),
            "README.md must not badge unrelated third-party crate ({forbidden})"
        );
    }

    let guide = std::fs::read_to_string(repo_root().join("docs/GUIDE.md")).expect("docs/GUIDE.md");
    assert!(guide.contains("![mayfly architecture](assets/architecture.svg)"));

    let contrib_path = repo_root().join("CONTRIBUTING.md");
    let contrib = std::fs::read_to_string(&contrib_path).expect("CONTRIBUTING.md exists");
    assert_clean_prose(&contrib_path, &contrib);
    assert_sanitized(&contrib_path, &contrib);
    for needle in [
        "1.91",
        "x86_64",
        "aarch64",
        "CONTRACT.md",
        "Signed-off-by",
        "cargo test --locked",
        "cargo build --locked --bin mayfly",
        "codepoint_scan",
        "clippy",
        "cargo doc --locked --no-deps",
        "SYNTHETIC-TEST-VALUE-",
        "[SECURITY.md](SECURITY.md)",
    ] {
        assert!(
            contrib.contains(needle),
            "CONTRIBUTING.md missing {needle:?}"
        );
    }

    fn valid_issue_form_yaml(text: &str) -> bool {
        if text.trim().is_empty() || text.contains('\t') {
            return false;
        }
        let mut block_indent: Option<usize> = None;
        for raw in text.lines() {
            if raw.trim().is_empty() || raw.trim_start().starts_with('#') {
                continue;
            }
            let indent = raw.len() - raw.trim_start().len();
            if indent % 2 != 0 {
                return false;
            }
            if let Some(min_indent) = block_indent {
                if indent > min_indent {
                    continue;
                }
                block_indent = None;
            }
            let trimmed = raw.trim();
            let rest = trimmed.strip_prefix("- ").unwrap_or(trimmed);
            let Some((_, val)) = rest.split_once(':') else {
                return false;
            };
            let v = val.trim();
            if v == "|" || v == ">" {
                block_indent = Some(indent);
                continue;
            }
            if v.chars().filter(|c| *c == '"').count() % 2 != 0 {
                return false;
            }
            if v.chars().filter(|c| *c == '[').count() != v.chars().filter(|c| *c == ']').count() {
                return false;
            }
        }
        true
    }
    assert!(!valid_issue_form_yaml("name: Bug report\nbroken: [\n"));
    assert!(!valid_issue_form_yaml("name: Bug report\n\tbad: true\n"));

    for (rel, markers) in [
        (
            ".github/ISSUE_TEMPLATE/bug_report.yml",
            &["name: Bug report", "SECURITY.md", "mayfly --version"][..],
        ),
        (
            ".github/ISSUE_TEMPLATE/feature_request.yml",
            &[
                "name: Feature or contract proposal",
                "SECURITY.md",
                "CONTRACT.md",
            ][..],
        ),
        (
            ".github/ISSUE_TEMPLATE/config.yml",
            &[
                "contact_links:",
                "https://github.com/david-engelmann/mayfly/security/advisories/new",
            ][..],
        ),
    ] {
        let p = repo_root().join(rel);
        let yml = std::fs::read_to_string(&p).unwrap_or_else(|_| panic!("{rel} exists"));
        assert_sanitized(&p, &yml);
        assert!(!yml.contains('\u{2014}') && !yml.contains('\u{2013}'));
        assert!(valid_issue_form_yaml(&yml), "{rel} failed YAML check");
        for m in markers {
            assert!(yml.contains(m), "{rel} missing {m:?}");
        }
    }
}
