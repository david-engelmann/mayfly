import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MARKER = "<!-- repo annex below this line -->"


def read(name):
    return (ROOT / name).read_text(encoding="utf-8")


def test_seed_files_exist():
    for name in ("README.md", "LICENSE", ".gitignore"):
        assert (ROOT / name).is_file(), name


def test_license_starts_with_mit_heading():
    assert read("LICENSE").startswith("MIT License\n")


def test_agents_has_marker_and_mayfly_annex_below_it():
    text = read("AGENTS.md")
    assert MARKER in text
    annex = text.split(MARKER, 1)[1]
    assert "mayfly" in annex
    assert "micro-VM" in annex


def test_agents_canonical_part_carries_program_headings():
    canonical = read("AGENTS.md").split(MARKER, 1)[0]
    assert canonical.startswith("# AGENTS.md\n")
    assert "## The round discipline" in canonical
    assert "## Contract first" in canonical
    assert "39 repo storefront perfection program" in canonical


def test_contract_names_five_setup_obligations():
    text = read("CONTRACT.md")
    for name in ("S1", "S2", "S3", "S4", "S5"):
        assert f"Obligation {name} covers" in text, name
    assert "Obligation S6" not in text


def test_readme_status_reports_zero_closed_rounds():
    text = read("README.md")
    assert "## Status" in text
    assert "0 of 100" in text
    assert "ca5df488b41c75114a08b752aaa0ed2eae255c92" in text


def test_ci_runs_the_local_gate_commands():
    text = read(".github/workflows/ci.yml")
    assert "python -m pytest -q" in text
    assert "python scripts/codepoint_scan.py" in text


def test_codepoint_scan_reports_zero_for_the_repo():
    result = subprocess.run(
        [sys.executable, str(ROOT / "scripts" / "codepoint_scan.py"), str(ROOT)],
        capture_output=True,
        text=True,
    )
    assert result.returncode == 0, result.stdout
    assert "count 0" in result.stdout
