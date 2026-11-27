# Contributing to Mayfly

Thank you for contributing to `mayfly`. Read [README.md](README.md), [CONTRACT.md](CONTRACT.md), and [AGENTS.md](AGENTS.md) before making changes so every pull request preserves the guarantees and conventions of the repository.

## Toolchain and prerequisites

Install Rust `1.91` or newer and Python `3.9` or newer. Full `NamespaceBackend` sandbox execution runs on Linux `x86_64` and `aarch64` hosts where unprivileged user namespaces are enabled, while `RunSpec` construction and pre-prepare bound and `--ro-bind` validation compile and run across platforms.

## Contract-first workflow

Every behavioral or surface change follows a strict commit sequence on a dedicated branch targeting `main`.

1. **Contract** states falsifiable obligations in [CONTRACT.md](CONTRACT.md) (backed by probes where external tool or kernel behavior is involved) before feature code is written.
2. **Build** implements the library, CLI, or documentation change that satisfies the stated obligation.
3. **Tests** adds discriminating unit or integration pins that fail when the claimed behavior is broken or negated.

Sign off every commit with a `Signed-off-by` trailer matching your Git author identity.

```
git commit -s -m "Build: concise summary of the change"
```

## Running the local gate

Every pull request must pass the full local gate with zero warnings before review.

```sh
cargo test --locked
cargo run --locked --bin codepoint_scan -- .
cargo fmt --all --check
cargo clippy --locked --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps
cargo build --locked --bin mayfly && cargo run --example quickstart && python3 examples/sandbox_walkthrough.py
```

## Copy, typography, and sanitization rules

- **Zero em or en dashes** across all tracked text, code, SVG, and YAML files (`U+2014` and `U+2013` are rejected by `codepoint_scan`).
- **Clean prose punctuation** in public Markdown and contract prose, with zero colons and zero semicolons in prose sentences outside fenced code blocks and inline code spans.
- **Strict sanitization** across code, tests, fixtures, examples, and documentation. Never include real credentials, session transcripts, personal machine paths, or private host details, and use clearly marked `SYNTHETIC-TEST-VALUE-*` strings in test fixtures and examples.

## Reporting security vulnerabilities

Do not open public issues or pull requests for suspected security vulnerabilities. Follow the private disclosure instructions in [SECURITY.md](SECURITY.md) instead.
