//! The setup pins, translated from the Python setup suite the repo
//! was seeded with, brought up to the current fleet standard. Each
//! test names the same fact its Python original named, with the
//! strengthenings the fleet folded in after mayfly's Round 1 suite
//! was written. Two facts changed with the Rust conversion and the
//! change is stated where it lands. The CI gate commands are the
//! Rust gate commands of amended obligation S2, and the contract
//! substance pin asserts the amended S2 phrases.

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

mod common;

use common::{mkfifo, root};

const MARKER: &str = "<!-- repo annex below this line -->";
const GATE_COMMAND: &str = "cargo test";
const SCAN_COMMAND: &str = "cargo run --bin codepoint_scan";
const FMT_COMMAND: &str = "cargo fmt --check";
const CLIPPY_COMMAND: &str = "cargo clippy --all-targets -- -D warnings";

fn read(name: &str) -> String {
    std::fs::read_to_string(root().join(name)).unwrap()
}

fn temp_dir(tag: &str) -> PathBuf {
    common::fresh_dir(tag)
}

#[test]
fn seed_files_exist() {
    for name in ["README.md", "LICENSE", ".gitignore"] {
        assert!(root().join(name).is_file(), "{name}");
    }
}

#[test]
fn license_starts_with_mit_heading() {
    assert!(read("LICENSE").starts_with("MIT License\n"));
}

#[test]
fn agents_has_marker_and_mayfly_annex_below_it() {
    let text = read("AGENTS.md");
    assert!(text.contains(MARKER));
    let annex = text.split(MARKER).nth(1).unwrap();
    assert!(annex.contains("mayfly"));
    assert!(annex.contains("CONTRACT.md"));
    assert!(annex.contains("micro-VM"));
}

#[test]
fn agents_canonical_part_carries_program_headings() {
    let text = read("AGENTS.md");
    let canonical = text.split(MARKER).next().unwrap();
    assert!(canonical.starts_with("# AGENTS.md\n"));
    assert!(canonical.contains("## The round discipline"));
    assert!(canonical.contains("## Contract first"));
    assert!(canonical.contains("39 repo storefront perfection program"));
}

#[test]
fn contract_names_five_setup_obligations() {
    let text = read("CONTRACT.md");
    for name in ["S1", "S2", "S3", "S4", "S5"] {
        assert!(
            text.contains(&format!("Obligation {name} covers")),
            "{name}"
        );
    }
    assert!(!text.contains("Obligation S6"));
}

fn status_section(text: &str) -> &str {
    let start = text.find("\n## Status\n").unwrap() + "\n## Status\n".len();
    let rest = &text[start..];
    match rest.find("\n## ") {
        Some(end) => &rest[..end],
        None => rest,
    }
}

#[test]
fn readme_status_states_one_hundred_of_one_hundred() {
    let text = read("README.md");
    assert!(text.contains("## Status"));
    let status = status_section(&text);
    // The full sentence, never the bare count, because the
    // bare count is a substring of every two digit count that
    // ends in the same digit and a pin on it would pass over
    // a wrong statement of the closed rounds.
    assert!(status.contains("Closed perfection rounds are 100 of 100."));
    assert!(status.contains("ca5df488b41c75114a08b752aaa0ed2eae255c92"));
}

fn flush_step(step_lines: &mut Vec<String>, commands: &mut Vec<String>) {
    if step_lines.is_empty() {
        return;
    }
    let mut disabled = false;
    let mut runs = Vec::new();
    for stripped in step_lines.iter() {
        if let Some(value) = stripped.strip_prefix("if:") {
            let value = value
                .trim()
                .trim_matches(|c| c == '\'' || c == '"')
                .to_lowercase();
            if value == "false" {
                disabled = true;
            }
        }
        if let Some(pos) = stripped.find("run:") {
            runs.push(stripped[pos + "run:".len()..].trim().to_string());
        }
    }
    if !disabled {
        commands.extend(runs);
    }
    step_lines.clear();
}

fn run_commands(ci_text: &str) -> Vec<String> {
    let mut commands = Vec::new();
    let mut step_lines: Vec<String> = Vec::new();
    let mut in_steps = false;
    let mut in_test_job = false;
    let mut job_disabled = false;
    for line in ci_text.lines() {
        let stripped = line.trim();
        if stripped.starts_with('#') {
            continue;
        }
        if stripped == "test:" {
            in_test_job = true;
            continue;
        }
        if in_test_job && !in_steps && stripped.starts_with("if:") {
            let value = stripped
                .split_once(':')
                .map(|(_, rest)| rest)
                .unwrap_or_default()
                .trim()
                .trim_matches(|c| c == '\'' || c == '"')
                .to_lowercase();
            if value == "false" {
                job_disabled = true;
            }
            continue;
        }
        if stripped == "steps:" {
            in_steps = true;
            continue;
        }
        if in_steps && stripped.starts_with("- ") {
            flush_step(&mut step_lines, &mut commands);
            step_lines.push(stripped[2..].trim().to_string());
            continue;
        }
        if in_steps {
            step_lines.push(stripped.to_string());
        }
    }
    flush_step(&mut step_lines, &mut commands);
    if job_disabled {
        return Vec::new();
    }
    commands
}

#[test]
fn ci_runs_the_local_gate_commands() {
    let commands = run_commands(&read(".github/workflows/ci.yml"));
    assert!(commands.iter().any(|c| c == GATE_COMMAND));
    assert!(commands.iter().any(|c| c == SCAN_COMMAND));
    assert!(commands.iter().any(|c| c == FMT_COMMAND));
    assert!(commands.iter().any(|c| c == CLIPPY_COMMAND));
}

fn toolchain_pins_all_full(text: &str) -> bool {
    let mut seen = false;
    for line in text.lines() {
        if let Some((_, pin)) = line.split_once("dtolnay/rust-toolchain@") {
            seen = true;
            let pin = pin.trim();
            if pin.len() != 40 {
                return false;
            }
            for b in pin.bytes() {
                if !b.is_ascii_digit() && !(b'a'..=b'f').contains(&b) {
                    return false;
                }
            }
        }
    }
    seen
}

#[test]
fn ci_carries_the_fleet_credential_hygiene() {
    let text = read(".github/workflows/ci.yml");
    // The workflow asks for no more than it uses. Its token
    // is limited to reading the repository, the checkout
    // leaves no credential behind for later steps, and the
    // toolchain action is pinned to a full commit SHA rather
    // than a moving branch name.
    assert!(text.contains("permissions:"));
    assert!(text.contains("contents: read"));
    assert!(text.contains("persist-credentials: false"));
    assert!(!text.contains("write-all"));
    for line in text.lines() {
        assert!(
            !line.trim_end().ends_with(": write"),
            "no workflow line may carry a write grant: {line}"
        );
    }
    assert!(!text.contains("rust-toolchain@master"));
    assert!(
        toolchain_pins_all_full(&text),
        "every toolchain step must carry a full commit SHA pin"
    );
}

#[test]
fn ci_toolchain_judgement_covers_every_toolchain_line() {
    // The hygiene pin judges the workflow's toolchain lines
    // through toolchain_pins_all_full. A judgement that read
    // only the first line would pass the second text below,
    // whose second toolchain step rides a moving branch name
    // the file wide master refusal does not name.
    let full = "0123456789abcdef0123456789abcdef01234567";
    let one = format!("      - uses: dtolnay/rust-toolchain@{full}\n");
    assert!(toolchain_pins_all_full(&one));
    let two_full = format!("{one}      - uses: dtolnay/rust-toolchain@{full}\n");
    assert!(toolchain_pins_all_full(&two_full));
    let second_moving = format!("{one}      - uses: dtolnay/rust-toolchain@main\n");
    assert!(!toolchain_pins_all_full(&second_moving));
    let second_short = format!("{one}      - uses: dtolnay/rust-toolchain@0123456789\n");
    assert!(!toolchain_pins_all_full(&second_short));
    assert!(!toolchain_pins_all_full("name: CI\n"));
}

#[test]
fn ci_gate_parser_skips_a_step_disabled_with_if_false() {
    let ci_text = read(".github/workflows/ci.yml");
    let gate_step = "      - name: Run the local gate\n        run: cargo test";
    assert!(ci_text.contains(gate_step));
    for flag in ["false", "\"false\""] {
        let disabled = ci_text.replace(
            gate_step,
            &format!(
                "      - name: Run the local gate\n        if: {flag}\n        run: cargo test"
            ),
        );
        let commands = run_commands(&disabled);
        assert!(!commands.iter().any(|c| c == GATE_COMMAND));
        assert!(commands.iter().any(|c| c == SCAN_COMMAND));
    }
}

#[test]
fn ci_gate_parser_rejects_a_job_disabled_with_if_false() {
    let ci_text = read(".github/workflows/ci.yml");
    let job_head = "  test:\n    runs-on: ubuntu-latest";
    assert!(ci_text.contains(job_head));
    for flag in ["false", "\"false\""] {
        let disabled = ci_text.replace(
            job_head,
            &format!("  test:\n    if: {flag}\n    runs-on: ubuntu-latest"),
        );
        let commands = run_commands(&disabled);
        assert!(!commands.iter().any(|c| c == GATE_COMMAND));
        assert!(!commands.iter().any(|c| c == SCAN_COMMAND));
        assert!(!commands.iter().any(|c| c == FMT_COMMAND));
        assert!(!commands.iter().any(|c| c == CLIPPY_COMMAND));
    }
    let commands = run_commands(&ci_text);
    assert!(commands.iter().any(|c| c == GATE_COMMAND));
    assert!(commands.iter().any(|c| c == SCAN_COMMAND));
    assert!(commands.iter().any(|c| c == FMT_COMMAND));
    assert!(commands.iter().any(|c| c == CLIPPY_COMMAND));
}

struct ScanOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

fn scan_tree(root: &Path) -> ScanOutput {
    let output = Command::new(env!("CARGO_BIN_EXE_codepoint_scan"))
        .arg(root)
        .output()
        .unwrap();
    ScanOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn scan_tree_with_extra(root: &Path, extra: &str) -> ScanOutput {
    let output = Command::new(env!("CARGO_BIN_EXE_codepoint_scan"))
        .arg(root)
        .arg(extra)
        .output()
        .unwrap();
    ScanOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn scan_from(dir: &Path) -> ScanOutput {
    let output = Command::new(env!("CARGO_BIN_EXE_codepoint_scan"))
        .current_dir(dir)
        .output()
        .unwrap();
    ScanOutput {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[test]
fn codepoint_scan_reports_zero_for_the_repo() {
    let result = scan_tree(&root());
    assert_eq!(result.status, 0, "{}", result.stdout);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

const CANONICAL_ABOVE_MARKER_SHA256: &str =
    "bb3e2fc1790b13729da3495c3f0902b968577f4e45ceb27e48e90b489f36e1a7";

#[test]
fn agents_canonical_part_matches_pinned_digest() {
    let agents = std::fs::read(root().join("AGENTS.md")).unwrap();
    let marker: &[u8] = b"<!-- repo annex below this line -->";
    let position = agents
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap();
    let mut hasher = Sha256::new();
    hasher.update(&agents[..position]);
    let digest: String = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    assert_eq!(digest, CANONICAL_ABOVE_MARKER_SHA256);
}

#[test]
fn readme_has_exactly_one_status_section() {
    let text = read("README.md");
    assert_eq!(text.matches("\n## Status\n").count(), 1);
}

#[test]
fn readme_carries_no_wrong_arc_label() {
    assert!(!read("README.md").contains("Fleet Operations"));
}

#[test]
fn codepoint_scan_counts_an_em_dash() {
    let dir = temp_dir("em-dash");
    std::fs::write(dir.join("note.md"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
    assert!(result.stdout.contains("note.md"));
}

#[test]
fn codepoint_scan_counts_an_en_dash() {
    let dir = temp_dir("en-dash");
    std::fs::write(dir.join("note.md"), "a\u{2013}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("en dash U+2013 count 1"));
}

#[test]
fn codepoint_scan_counts_both_dashes_in_one_tree() {
    // MF-P34. Every staged tree in this suite carries at
    // most one dash kind in any one file, so the scanner's
    // two tallies stand cross unpinned: a counting loop that
    // counted each dash in both tallies would print both
    // totals raised and pass every pin standing, and a per
    // file line carrying its two counts swapped or dropped
    // passes likewise, the one pin that reads for a file's
    // name asserting the name alone. This pin stages both
    // kinds in one tree and reads the whole output, so it
    // fails on the double counting form at the totals, on a
    // swap at the totals and the lines alike, and on a per
    // file line that drops or swaps its counts or prints
    // out of the standing order.
    let dir = temp_dir("both-dashes");
    std::fs::write(dir.join("alpha.md"), "a\u{2014}b\u{2014}c\u{2013}d").unwrap();
    std::fs::write(dir.join("beta.md"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    let expected = format!(
        "{}: em dash 2, en dash 1\n{}: em dash 1, en dash 0\nem dash U+2014 count 3\nen dash U+2013 count 1\n",
        dir.join("alpha.md").display(),
        dir.join("beta.md").display()
    );
    assert_eq!(result.stdout, expected);
}

#[test]
fn codepoint_scan_defaults_to_the_working_directory() {
    // MF-P35. Every other drive in this suite names its root
    // on the command line, while the gate's own scan step
    // names none and runs on the default from the checkout
    // directory, so the default stands driven by the gate
    // alone and pinned by nothing. A scanner defaulting to
    // its manifest directory, or to the root of the repo it
    // ships in as the setup scanner did, would scan this
    // repo's own tree from any working directory, and that
    // tree stands at zero counts, so the dash drive below
    // would answer 0 with both counts at zero. A scanner
    // refusing without an argument answers at exit 2. This
    // pin drives the bare form from two planted working
    // directories and holds both answers.
    let dir = temp_dir("default-root");
    std::fs::write(dir.join("note.md"), "a\u{2014}b").unwrap();
    let result = scan_from(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
    let clean = temp_dir("default-root-clean");
    std::fs::write(clean.join("note.md"), "plain text").unwrap();
    let result = scan_from(&clean);
    assert_eq!(result.status, 0);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

#[test]
fn codepoint_scan_scans_a_root_under_an_excluded_ancestor() {
    let dir = temp_dir("excluded-ancestor").join("venv").join("tree");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("note.md"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
}

#[test]
fn codepoint_scan_skips_excluded_dirs_below_the_root() {
    let dir = temp_dir("excluded-dirs");
    let cache = dir.join("build");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(cache.join("note.md"), "a\u{2014}b").unwrap();
    std::fs::write(dir.join("clean.md"), "clean").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 0);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
}

#[test]
fn codepoint_scan_skips_every_excluded_dir_below_the_root() {
    let dir = temp_dir("excluded-dirs-all");
    std::fs::write(dir.join("clean.md"), "clean").unwrap();
    for name in [
        ".git",
        "__pycache__",
        ".pytest_cache",
        ".venv",
        "venv",
        "node_modules",
        "build",
        "dist",
        "target",
    ] {
        let excluded = dir.join(name);
        std::fs::create_dir_all(&excluded).unwrap();
        std::fs::write(excluded.join("note.md"), "a\u{2014}b").unwrap();
    }
    let result = scan_tree(&dir);
    assert_eq!(result.status, 0);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
    // The control. The same dash standing in a real directory
    // inside a second root counts once, so the zeros are the
    // exclusions' doing and not a scan that read nothing. A
    // list re-taken from memory dropping any one of the nine
    // names counts that name's file and fails the zeros above.
    let control = temp_dir("excluded-dirs-all-control");
    let inside = control.join("real-dir");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(inside.join("note.md"), "a\u{2014}b").unwrap();
    let result = scan_tree(&control);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
}

#[test]
fn contract_obligations_carry_their_substance() {
    let text = read("CONTRACT.md");
    for phrase in [
        "present from the first commit",
        "every later change lands through a pull request",
        "no paid service",
        "cargo test plus the codepoint scan run through cargo",
        "0 of 100 until a full cycle",
        "no file carries a real trace or a real transcript or a real name",
        "SHA-256 digest of the part above the marked line",
    ] {
        assert!(text.contains(phrase), "{phrase}");
    }
}

#[test]
fn codepoint_scan_fails_loudly_on_undecodable_input() {
    let dir = temp_dir("undecodable");
    std::fs::write(dir.join("note.md"), b"\xff\xfe not utf-8").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 2);
    assert!(result.stderr.contains("note.md"));
}

#[test]
fn codepoint_scan_reports_a_failed_output_write_as_a_scan_error() {
    // The scanner's exits are 0, 1, and 2, and a write of
    // its own output that fails is a scan error at exit 2,
    // in the same voice as a file that cannot be read.
    // /dev/full refuses every write, so the first per file
    // line over a tree carrying one em dash already fails.
    // Against printing through println! the scanner panics
    // at exit 101 there, outside its own vocabulary, and
    // this pin fails on that code. The staging is proved
    // by common::full_device before the drive, and the pin
    // skips where the host's /dev/full is not the failing
    // device.
    let dir = temp_dir("scan-write-fails");
    std::fs::write(dir.join("note.md"), "a\u{2014}b").unwrap();
    let Some(full) = common::full_device() else {
        return;
    };
    let output = Command::new(env!("CARGO_BIN_EXE_codepoint_scan"))
        .arg(&dir)
        .stdout(std::process::Stdio::from(full))
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("scan error"));
}

#[test]
fn codepoint_scan_counts_a_dash_in_a_file_with_no_suffix() {
    let dir = temp_dir("no-suffix");
    std::fs::write(dir.join("LICENSE"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
}

#[test]
fn codepoint_scan_counts_a_dash_in_an_uppercase_suffix_file() {
    let dir = temp_dir("upper-suffix");
    std::fs::write(dir.join("UPPER.MD"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
}

#[test]
fn codepoint_scan_skips_a_file_with_an_uppercase_binary_suffix() {
    let dir = temp_dir("upper-binary");
    std::fs::write(dir.join("FAKE.PNG"), "a\u{2014}b").unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 0);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

#[test]
fn codepoint_scan_skips_every_binary_suffix() {
    let dir = temp_dir("binary-suffixes-all");
    for suffix in [
        "png", "jpg", "jpeg", "gif", "ico", "webp", "pdf", "zip", "gz", "tar", "whl", "so", "dylib",
    ] {
        std::fs::write(dir.join(format!("sample.{suffix}")), "a\u{2014}b").unwrap();
    }
    let result = scan_tree(&dir);
    assert_eq!(result.status, 0);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
    // The control. A dash file standing at the root of a
    // second tree counts once, so the zeros are the skips'
    // doing and not a scan that read nothing. A list re-taken
    // from memory dropping any one of the thirteen suffixes
    // counts that suffix's file and fails the zeros above.
    let control = temp_dir("binary-suffixes-all-control");
    std::fs::write(control.join("note.md"), "a\u{2014}b").unwrap();
    let result = scan_tree(&control);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
}

fn scan_tree_bounded(root: &Path, timeout_secs: u64) -> ScanOutput {
    let (tx, rx) = std::sync::mpsc::channel();
    let root = root.to_path_buf();
    std::thread::spawn(move || {
        let _ = tx.send(scan_tree(&root));
    });
    rx.recv_timeout(std::time::Duration::from_secs(timeout_secs))
        .expect("the scan did not finish inside its bound")
}

#[test]
fn codepoint_scan_completes_over_a_fifo_in_the_tree() {
    // A pipe standing in a scanned tree is debris, not
    // content. A scan that collects it blocks on the read
    // until a writer appears, which is to say forever, so the
    // scan runs here under a bound and the pin fails on the
    // bound instead of hanging the suite. The scan completes
    // with the clean file's zero counts.
    let dir = temp_dir("fifo");
    std::fs::write(dir.join("clean.md"), "clean").unwrap();
    mkfifo(&dir.join("planted-pipe"));
    let result = scan_tree_bounded(&dir, 30);
    assert_eq!(result.status, 0, "{}", result.stderr);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

#[test]
fn codepoint_scan_completes_over_a_dangling_symlink() {
    // A link whose target is gone is debris as well. A scan
    // that collects it fails the whole scan at exit 2 on a
    // read that cannot start, over a file that was never
    // content. The scan completes with zero counts.
    let dir = temp_dir("dangling-symlink");
    std::fs::write(dir.join("clean.md"), "clean").unwrap();
    std::os::unix::fs::symlink(dir.join("no-such-target"), dir.join("dangling.md")).unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 0, "{}", result.stderr);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
}

#[test]
fn codepoint_scan_counts_a_dash_behind_a_symlink_to_a_regular_file() {
    // A link to a regular file is content the scan has always
    // read through, and the regular file rule keeps it. Both
    // the file and the link are scanned, so the dash counts
    // twice, and a fix that skipped every link would fail here.
    let dir = temp_dir("symlink-file");
    let target = dir.join("real.md");
    std::fs::write(&target, "a\u{2014}b").unwrap();
    std::os::unix::fs::symlink(&target, dir.join("linked.md")).unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 2"));
}

#[test]
fn codepoint_scan_does_not_follow_a_symlink_to_a_directory() {
    // A link whose target is a directory is neither content
    // nor a door into the tree behind it. The collect rule
    // takes a regular file, or a link whose target is one,
    // and the walk descends only into entries whose own type
    // is a directory, so the link is left whole, exactly as
    // the scanner's header promises. A walk that descended
    // through the link would count text standing outside the
    // tree the caller named, and a link pointing at an
    // ancestor of itself would walk without bound. A collect
    // that took the link itself as a file would fail the scan
    // at exit 2 on a read that cannot start. The first drive
    // fails on its counts under the descending form and on
    // its status under the collecting form.
    let outside = temp_dir("symlink-dir-outside");
    std::fs::write(outside.join("note.md"), "a\u{2014}b").unwrap();
    let dir = temp_dir("symlink-dir");
    std::fs::write(dir.join("clean.md"), "clean").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("linked-dir")).unwrap();
    let result = scan_tree(&dir);
    assert_eq!(result.status, 0, "{}", result.stderr);
    assert!(result.stdout.contains("em dash U+2014 count 0"));
    assert!(result.stdout.contains("en dash U+2013 count 0"));
    // The control. The same dash standing in a real directory
    // inside a second root counts once, so the zero behind
    // the link is the link's doing and not a scan that reads
    // nothing. A walk that descended nowhere fails here.
    let control = temp_dir("symlink-dir-control");
    let inside = control.join("real-dir");
    std::fs::create_dir_all(&inside).unwrap();
    std::fs::write(inside.join("note.md"), "a\u{2014}b").unwrap();
    let result = scan_tree(&control);
    assert_eq!(result.status, 1);
    assert!(result.stdout.contains("em dash U+2014 count 1"));
}

#[test]
fn codepoint_scan_refuses_an_argument_after_the_root() {
    let dir = common::Tmp::new("extra-argument");
    std::fs::write(dir.path().join("clean.md"), "clean").unwrap();
    let result = scan_tree_with_extra(dir.path(), "second-root");
    assert_eq!(result.status, 2);
    assert!(result.stdout.is_empty(), "{}", result.stdout);
    assert!(result.stderr.contains("second-root"), "{}", result.stderr);
}

#[test]
fn codepoint_scan_refuses_an_argument_after_a_dirty_root() {
    let dir = common::Tmp::new("extra-argument-dirty");
    std::fs::write(dir.path().join("note.md"), "a\u{2014}b").unwrap();
    let result = scan_tree_with_extra(dir.path(), "second-root");
    assert_eq!(result.status, 2);
    assert!(result.stdout.is_empty(), "{}", result.stdout);
    assert!(result.stderr.contains("second-root"), "{}", result.stderr);
}

#[test]
fn codepoint_scan_extra_argument_on_dead_stderr_exits_two() {
    let dir = common::Tmp::new("extra-argument-dead-stderr");
    std::fs::write(dir.path().join("clean.md"), "clean").unwrap();
    let Some(full) = common::full_device() else {
        return;
    };
    let status = Command::new(env!("CARGO_BIN_EXE_codepoint_scan"))
        .arg(dir.path())
        .arg("second-root")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::from(full))
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(2));
}

#[test]
fn portability_workflow_carries_the_fleet_credential_hygiene() {
    let port_text = read(".github/workflows/portability.yml");
    assert!(port_text.contains("permissions:\n  contents: read"));
    assert!(!port_text.contains("write-all"));
    assert!(!port_text.contains("contents: write"));
    let checkouts = port_text.matches("actions/checkout@").count();
    assert!(checkouts >= 1);
    assert_eq!(
        port_text.matches("persist-credentials: false").count(),
        checkouts
    );
    assert!(!port_text.contains("rust-toolchain@master"));
    assert!(
        toolchain_pins_all_full(&port_text),
        "every toolchain step in portability.yml must carry a full commit SHA pin"
    );
}

#[test]
fn codepoint_scan_refuses_a_root_that_does_not_stand() {
    // The scanner's header names a scan root that does not
    // stand as a scan error at exit 2, one of its deliberate
    // differences from the scanner it ports, which exited 0
    // with zero counts. A form that read a missing root as
    // an empty tree would answer zero counts at exit 0, and
    // the gate step that runs this scanner over the repo
    // would report a clean scan of nothing. A regular file
    // standing as root fails the same directory read and
    // draws the same answer, with the root named on the
    // error stream both times.
    let dir = temp_dir("missing-root");
    let missing = dir.join("no-such-scan-root");
    let result = scan_tree(&missing);
    assert_eq!(result.status, 2);
    assert!(
        result.stderr.contains("no-such-scan-root"),
        "{}",
        result.stderr
    );
    let file_root = dir.join("a-file-not-a-tree");
    std::fs::write(&file_root, "clean").unwrap();
    let result = scan_tree(&file_root);
    assert_eq!(result.status, 2);
    assert!(
        result.stderr.contains("a-file-not-a-tree"),
        "{}",
        result.stderr
    );
}
