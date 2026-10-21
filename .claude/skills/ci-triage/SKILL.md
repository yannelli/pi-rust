---
name: ci-triage
description: Find the cause of a failed or hung CI or Release run in pi-rust and fix it. Use when a PR check fails, a test is flaky, a job runs much longer than usual, or a release did not publish.
---

# CI triage

Full playbook: `docs/agents/ci-triage.md`.

1. List the jobs: `gh run view <run-id> -R yannelli/pi-rust --json jobs --jq '.jobs[]|[.name,.status,.conclusion,.databaseId]|@tsv'`.
2. Read a finished job's log while the run continues: `gh api --allow-escape-sequences repos/yannelli/pi-rust/actions/jobs/<job-id>/logs`. Strip the color codes and grep for `panicked|FAILED|timed out|error`.
3. A job far past its usual time is hung: cancel the run to get the logs.
4. Compare the symptom with the Windows table in the playbook. Most Windows-only failures so far were paths, named pipes, handle passing, or code that ran before `process` was ready.
5. Reproduce locally. For a race, stress the test binary with the loop in the playbook before and after the fix.
6. Fix the cause. Do not add retries or loosen assertions to hide a race.

For a Release run that failed outside the code (a download or runner error), `gh run rerun <run-id> --failed`.
