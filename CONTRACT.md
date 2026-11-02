# Contract

Mayfly asks whether an agent task can run inside a disposable micro-VM that spins up fast, runs in isolation, and is destroyed the moment the task ends. This file is the stub contract for the setup slice. It states the setup obligations only, and each obligation is stated so it can fail.

Obligation S1 covers seed integrity. The seed files README.md and LICENSE and .gitignore are present from the first commit, and every later change lands through a pull request rather than a direct push to main. This is covered by inspection of the repository history and by the status section of the README.

Obligation S2 covers the local gate. The test suite runs with python and pytest alone and it needs no network access and no paid service, and the continuous integration workflow runs the same commands as the local gate. This is covered by the tests in the tests folder and by the workflow in the .github folder.

Obligation S3 covers honest status. The README status section records the seed state and the contract state truthfully and it reports closed perfection rounds as 0 of 100 until a full cycle of work and audit and review and red team and approval has closed. This is covered by inspection of the README against this contract and against the repository history.

Obligation S4 covers synthetic content only. Every fixture and every example in this repository is synthetic and written for this repo, and no file carries a real trace or a real transcript or a real name or any private operational detail. This is covered by inspection of the repository contents before any push.

Obligation S5 covers the canonical agents file. The part of AGENTS.md above the marked line is identical to the canonical template for the program, and only the repo annex below the marked line differs for mayfly. This is covered by tests in the tests folder that pin the SHA-256 digest of the part above the marked line and check the marked line and the annex.

The full contract for mayfly is the first build act of this repo and it lands with the first slice. This stub is not the full contract, and no feature claim for mayfly rests on this stub.
