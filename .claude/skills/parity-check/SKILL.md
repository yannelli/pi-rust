---
name: parity-check
description: Find gaps between pi-rust and upstream pi (earendil-works/pi). Use when asked what is missing, whether pi-rust matches upstream, or to review the daily parity report.
---

# Parity check

Full playbook: `docs/parity/README.md`.

1. Get an up-to-date upstream checkout:
   `git clone https://github.com/earendil-works/pi ../pi-upstream` once, then `git -C ../pi-upstream pull --ff-only`.
2. Run `cargo xtask parity --upstream ../pi-upstream --out parity-report.md`.
3. Read the summary table, then check each finding in both codebases. The report compares text, so a rename or a moved file shows up as a gap.
4. Sort the confirmed findings:
   - Upstream commits to port: note the crate and the size of the change.
   - Accepted differences: add to `docs/parity/allowlist.json` with a reason.
   - Gaps that need a decision or a plan: add to `docs/parity/GAPS.md`.
5. Report the confirmed gaps, the false positives (and which parser caused them), and the next port to do.

Do not port code in the same pass unless the user asks. For many findings, give the `parity-auditor` subagent one section at a time.
