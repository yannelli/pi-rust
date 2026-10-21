# Daily gap check (runner prompt)

Use this file as the prompt for a scheduled agent, such as a Claude Code routine, a Codex cloud task, or a Cursor background agent. The [Parity workflow](../../.github/workflows/parity.yml) already posts the raw report every day, and the agent checks it and keeps the gap register correct.

## Prompt

You are checking pi-rust (github.com/yannelli/pi-rust) for gaps against upstream pi (github.com/earendil-works/pi). Read `AGENTS.md` and `docs/parity/README.md` first.

1. Get the latest report from the open issue labeled `parity-report` (`gh issue list -R yannelli/pi-rust -l parity-report`). If there is none, clone upstream and run `cargo xtask parity --upstream <upstream> --out parity-report.md`.
2. Compare it with `docs/parity/GAPS.md` and list the findings that are new since the register was last reviewed.
3. Check at most 10 new findings in the code of both repositories. Drop false positives and note which parser caused them.
4. For each upstream commit, read `git show <sha>` upstream. Say which crate the change belongs to and how large the port is (one file, one crate, or several crates).
5. Check CI health. Look for failed runs on `main` in the last day (`gh run list -R yannelli/pi-rust -b main -s failure -L 10`) and check that the latest release has all 9 assets.
6. Update `docs/parity/GAPS.md`. Add the confirmed findings to the right section, remove entries that are now closed, and set the "Last reviewed" line. Put a false positive in `docs/parity/allowlist.json` only when it is an accepted difference, and report parser bugs instead of hiding them.
7. Open a PR named `docs(parity): gap review <date>` with only those doc changes. Do not port code, merge, or release in this run.

Output a summary of at most 15 lines with the new gaps (with upstream SHA or API name), the gaps closed since the last review, CI or release problems, and parser bugs.

## Limits

- Use no AI provider keys and run no evals.
- Do not push to `main`, merge PRs, or edit versions or `CHANGELOG.md`.
- If the report or CI is broken, say so and stop.
