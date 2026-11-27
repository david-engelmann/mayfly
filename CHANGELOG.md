# Changelog

All notable changes to mayfly are recorded in this file.

## 0.2.0 on 2026-11-26

- Dedicated `tmpfs` with mode `01777` mounted at `/tmp` inside every prepared sandbox root so scratch files under `/tmp` stay private to the namespace and vanish on exit under `MF-14`.
- Caller-granted read-only host directory and regular-file binds (`ro_binds` on `RunSpec` and repeatable `--ro-bind`) added with pre-prepare `symlink_metadata` validation, byte-exact non-UTF-8 path mounting, and recursive `mount_setattr` / `remount_read_only_tree` read-only locking before `pivot_root` under `MF-14`.
- Top-level `-h`, `--help`, `-V`, and `--version` command line flags added under `MF-14`.
- Unix build portability across Linux and macOS, portability CI, and automated `v0.2.0` Linux `x86_64` and `aarch64` release workflows added under `MF-15`.

## 0.1.0 on 2026-11-15

- Disposable Linux namespace sandbox harness (`user`, `mount`, `pid`, `uts`, `ipc`, and default `net` isolation) with `pivot_root`, dropped task capabilities, scrubbed environment, and measured JSON run reports (`MF-1` through `MF-12`).
- Janitor process, orphan sweep, parent death signals, and caller-configured wall-clock timeout, processor time ceiling, and address-space ceiling (`MF-13`).
