# Security Policy

## Supported Versions

Security fixes land on the `0.2.x` release line. Versions prior to `0.2.0` do not receive security patches.

## Security Model

Mayfly isolates one task per disposable Linux namespace sandbox (`user`, `mount`, `pid`, `uts`, `ipc`, and default `net` namespaces), pivots into a prepared root with host system trees and any `--ro-bind` inputs locked read-only, mounts private `tmpfs` filesystems at `/work` and `/tmp`, and drops all capabilities in the forked task child before `exec`. Mayfly is a Linux namespace sandbox and not a hypervisor or micro-VM, so its isolation boundary rests on the host Linux kernel.

## Reporting a Vulnerability

Do not open a public issue for a suspected security vulnerability. Report vulnerabilities privately through [GitHub Security Advisories](https://github.com/david-engelmann/mayfly/security/advisories/new) with a minimal synthetic reproduction, the Linux kernel version, the affected mayfly version, and the contract obligation involved. Every report receives an initial review within seven days, and confirmed fixes ship with a regression pin in the test suite before public disclosure.
