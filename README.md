# mayfly

A disposable micro-VM sandbox for agent tasks. It fires up fast, runs the task, and incinerates itself on exit.

## What this is

Agents run code they just wrote, on your machine, with your credentials nearby. Mayfly gives that code somewhere else to live. Each task gets a fresh micro-VM that spins up in under 200ms, optionally with a GPU attached, runs in isolation, and is destroyed the moment the task ends. Nothing persists unless you ask for it. The VM is a mayfly by design. It lives for one job and dies without ceremony.

## Status

Seeded on 2026-10-29. The concept is claimed and the build has not started.

## Licence

MIT. See LICENSE.
