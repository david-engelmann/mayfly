# AGENTS.md

This repo is one of the repos in David Engelmann's 39 repo storefront perfection program. Every repo in the program runs the same operating discipline, written here once. The repo annex below the marked line at the end of this file carries what is specific to this repo. Everything above that line is the canonical template and is identical in every repo in the program. Never edit the canonical part inside one repo. A change to the canonical part is made once in the canonical template and then applied to every repo.

## What this repo is

The repo's own README and CONTRACT.md define the question this repo answers and the boundary it holds against its neighbours in the program. Read both before changing anything. Where the contract and the code disagree, the contract is the spec and the code is the suspect, until a probe against the real tool or runtime the contract names settles which one is wrong.

## The round discipline

The program runs 100 rounds per repo. One round is one full cycle of work, audit, two axis review, red team, fixes, and owner approval, with the approval recorded. Research passes, test runs, review sub steps, and log entries are never rounds, and nothing is ever reported as a closed round that did not run the full cycle. Closed rounds are counted in the README status and in the program's rounds registry, and the two always agree.

Work inside a round lands in the established commit order. The contract first, with the round's obligations written before any feature code. Then the build. Then the tests. Then any regenerated examples. Then the status. Every commit carries the trailer Signed-off-by: David Engelmann <81196676+david-engelmann@users.noreply.github.com>.

## Contract first

Every slice states its obligations in CONTRACT.md before code is written, and each obligation is stated so it can fail. An obligation the code cannot violate is not an obligation. Where a reading rests on the behaviour of a real tool or runtime, probe that behaviour first and write the probe findings into the contract preamble, so the obligations rest on evidence rather than on habit. Where a later round retires a limit an earlier round stated, the retirement is written as an amendment that names the earlier text, and the earlier text is left standing so the record shows both.

## Gates

The local gate is the repo's full test suite, run on the exact final head, in the worktree and again from a fresh archive extraction of the committed tree. Every gate run writes its output to a file and records its exit code separately. A pipeline exit code is never trusted, because a pipeline reports the last command in it and not the test run. CI is a backstop, not a gate. A change merges on green local gates plus the three reviews, and never merges while its own CI is known red for its own reasons. If CI and the local gate disagree on the same head, stop and find out why before anything else lands.

Heavy builds and full gates on the shared machine are serialized through the program's build slot. Claim the slot before a full gate and release it when the gate finishes. Develop against targeted checks and run the full gate once, at checkpoint time.

## The three reviews

Before a round closes, the finished state gets three reviews. An independent red team re-audit, from fresh eyes, trying to break every claim against the code and against the real tool or runtime where the claim rests on one. An independent line by line review of the actual diff on two parallel axes that are reported separately and never merged, spec fidelity against the contract's obligations, and standards against this file and the repo's conventions. And the owner's own verification, re-running the key commands and re-checking the key claims rather than trusting any lane's report of them. Fix everything the reviews find before the round is recorded as closed.

Review lanes are read only. A reviewer never edits, commits, or writes inside the repo under review, and never records a round closure. The shared rounds registry and the status files belong to the owner chat alone.

CodeRabbit is advisory step 0 on every pull request. On a repo under ten stars the automatic review is skipped, so trigger it with an @coderabbitai review comment, within its allowance of one included review per hour. Read every finding, fix what is valid, and dismiss the rest silently with a one line reason in the gate report. CodeRabbit approval is never a review pass and CodeRabbit silence is never a blocker.

## Tests

Every test must be able to fail for a real reason. A test that passes under both the claimed behaviour and its negation proves nothing and does not count as a pin. Every load bearing rule in a new obligation carries a discriminating content level pin, at the edges of the rule as well as at its core. When a fix repairs behaviour, validate the new pins against the pre fix code where that is possible, and say in the gate report which pins fail there. Existing tests are altered only for a named mechanical consequence of the change or a recorded correction, with the justification written where the alteration lands.

## Copy and typography

Public copy and contract prose carry no em dashes and no en dashes, and no colons and no semicolons in prose sentences. Verify character claims with a Python codepoint scan. A shell grep for a Unicode character can match nothing and exit clean while the characters stand everywhere, so a zero hits result from grep is never evidence. Never present copy as final until it has been re-read end to end against this bar.

## Sanitization

Nothing real enters this repo. No real traces, transcripts, session data, names, addresses, paths from a working machine, or private operational detail, in code, tests, fixtures, examples, or prose. Every fixture is synthetic and written for this repo. Results measured against real corpora are reported as numbers only and are never committed.

## Git discipline

All work lands through pull requests. Main is protected and takes no direct pushes. One branch per slice, stacked on its parent branch when the work is stacked, opened as a pull request as soon as the branch is pushed. One live pull request per work unit. Worktrees live under the workspace and never under /tmp, which is small, ephemeral, and wiped without warning. Commit early and often, and never hand off with uncommitted work. Pushes through the Git Data API carry the complete file list for the tree being built, and the pushed tree is verified against the local tree before the pull request opens.

## Honesty of record

Verify claims from the thing itself. A result is read from the file on disk, the rendered surface, or the app state, never from a summary, a count, a hash, or another lane's say so. When a number is reported, it was counted from the artifact it describes. When a gate is reported green, its exit code was recorded separately from its output. When something fails, the failure is stated plainly in the same report as the successes, with the evidence. A blocker is carried only after the routes its own record lists have been tried, and a carried blocker is re-tried, not re-verified, when work resumes.

<!-- repo annex below this line -->

## Repo annex

The contract for this repo lives in CONTRACT.md and it defines the obligations this repo must meet. Read CONTRACT.md and the README before changing anything in mayfly.

Mayfly makes the box, switchyard furnishes it, and wayside remembers where everything was. Mayfly is a disposable sandbox for agent tasks. One fresh box is built for one agent task and incinerated on exit, and what a run measurably did lives in the run report that run prints.

The first slice of mayfly is a lifecycle harness whose first backend is a Linux namespace sandbox. The harness builds a box, runs one task inside it, and incinerates the box, with the timings measured per run and published in the run report. The lifecycle interface stays open to container or micro-VM backends in later rounds, where those runtimes exist. There is no custom hypervisor in slice one.
