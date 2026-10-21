# Release guide

Releases are automatic. Do not edit versions or `CHANGELOG.md` by hand.

## Flow

1. Work on a branch and open a PR to `main`. Use Conventional Commits; they set the version bump (see the table in the [README](../../README.md#releases)).
2. CI must pass on Linux, macOS, and Windows before you merge.
3. After the merge, CI runs on `main`. When it passes, the Release workflow runs `cargo xtask plan`. If a release is due it commits `chore(release): vX.Y.Z`, tags it, builds every target, and publishes the release.
4. Check the result:

```sh
gh release list -R yannelli/pi-rust -L 3
gh release view <tag> -R yannelli/pi-rust --json assets --jq '.assets[].name'
```

A complete release has 9 assets: 7 `pi-<tag>-<target>` archives (x86_64 and aarch64 Linux musl, armv7 Linux musl, x86_64 and aarch64 macOS, x86_64 and aarch64 Windows), `SHA256SUMS`, and `pi-env-daemons-<tag>.tar.gz`.

## When it fails

- A build or daemon job failed: read its log ([CI triage](ci-triage.md)). For a download or runner error, `gh run rerun <run-id> --failed`. The publish job runs after the rerun passes.
- The Android NDK download is retried three times in the workflow. If it still fails, rerun the failed job.
- Never publish by hand and never move a tag. Fix the cause and let the workflow release the next version.

## Dry run

Run the Release workflow by hand with `dry_run` set to build every target without tagging or publishing:

```sh
gh workflow run Release -R yannelli/pi-rust -f dry_run=true
```
