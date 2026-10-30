# Daily gap check (runner prompt)

Use this file as the prompt for a scheduled agent. Examples: a Claude Code routine, a Codex cloud task, or a Cursor background agent. The [Parity workflow](../../.github/workflows/parity.yml) posts the raw report every day. The agent does a check of the report and keeps the gap register correct.

## Prompt

Your task is to find gaps between pi-rust (github.com/yannelli/pi-rust) and upstream pi (github.com/earendil-works/pi). First, read `AGENTS.md` and `docs/parity/README.md`.

1. Get the latest report from the open issue with the label `parity-report` (`gh issue list -R yannelli/pi-rust -l parity-report`). If there is no report, run it locally as in [the playbook](../parity/README.md#daily-gap-check). Include the model data step.
2. Compare the report with `docs/parity/GAPS.md`. List the findings that are new since the last review of the register.
3. Check at most 10 new findings in the code of both repositories. Remove false positives. Record which parser caused each false positive.
4. For each upstream commit, read `git show <sha>` upstream. Write which crate the change belongs to. Write the size of the port: one file, one crate, or several crates.
5. Check CI health. Look for failed runs on `main` in the last day (`gh run list -R yannelli/pi-rust -b main -s failure -L 10`). Make sure that the latest release has all 9 assets.
6. Update `docs/parity/GAPS.md`. Add the confirmed findings to the correct section. Remove entries that are now closed. Set the "Last reviewed" line. Put a false positive in `docs/parity/allowlist.json` only when it is an accepted difference. Report parser bugs, and do not hide them.
7. Open a PR named `docs(parity): gap review <date>` with only those doc changes. Do not port code, merge, or release in this run. Do not write `upstream <sha>` in the commit message, because the report reads that text as a port.

Output a summary of at most 15 lines. List the new gaps, each with its upstream SHA or API name. Then list the gaps closed since the last review, CI or release problems, and parser bugs.

## Limits

- Do not use AI provider keys. Do not run evals.
- Do not push to `main`, merge PRs, or edit versions or `CHANGELOG.md`.
- If the report or CI is broken, report the problem and stop.
