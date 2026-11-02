# mayfly

A disposable micro-VM sandbox for agent tasks. It fires up fast, runs the task, and incinerates itself on exit.

## What this is

Agents run code they just wrote, on your machine, with your credentials nearby. Mayfly gives that code somewhere else to live. Each task gets a fresh micro-VM that spins up in under 200ms, optionally with a GPU attached, runs in isolation, and is destroyed the moment the task ends. Nothing persists unless you ask for it. The VM is a mayfly by design. It lives for one job and dies without ceremony.

## Status

The remote seed was verified on 2026-11-02 against the staged seed and its commit is ca5df488b41c75114a08b752aaa0ed2eae255c92, a parentless commit holding exactly README.md and LICENSE and .gitignore. Setup is landing through a pull request and no work lands on main directly.

The contract lives in CONTRACT.md. Setup obligations S1 to S5 are the contract state until the full contract lands as the first build act of this repo, and this setup slice claims no feature behaviour for mayfly.

Closed perfection rounds are 0 of 100. A round counts only when a full cycle of work and audit and review and red team and approval has closed, and seeding and setup are not rounds.

The local gate is python -m pytest -q plus python scripts/codepoint_scan.py, run on the exact final head before any push, with the continuous integration workflow running the same commands as a backstop.

## Licence

MIT. See LICENSE.
