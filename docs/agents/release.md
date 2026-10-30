# Release guide

Releases are automatic. Do not edit versions or `CHANGELOG.md` by hand.

## Flow

1. Work on a branch. Open a PR to `main`. Use Conventional Commits, because they set the version bump. Refer to the table in the [README](../../README.md#releases).
2. CI must pass on Linux, macOS, and Windows before you merge.
3. After the merge, CI runs on `main`. When it passes, the Release workflow runs `cargo xtask plan`. If a release is due, the workflow commits `chore(release): vX.Y.Z` and tags the commit. Then it builds every target and publishes the release.
4. Check the result:

```sh
gh release list -R yannelli/pi-rust -L 3
gh release view <tag> -R yannelli/pi-rust --json assets --jq '.assets[].name'
```

A complete release has 9 assets: 7 `pi-<tag>-<target>` archives, `SHA256SUMS`, and `pi-env-daemons-<tag>.tar.gz`. The 7 archive targets are x86_64 and aarch64 Linux musl, armv7 Linux musl, x86_64 and aarch64 macOS, and x86_64 and aarch64 Windows.

## When it fails

- If a build or daemon job failed, read its log ([CI triage](ci-triage.md)). For a download or runner error, run `gh run rerun <run-id> -R yannelli/pi-rust --failed`. The publish job runs after the rerun passes.
- The workflow retries the Android NDK download three times. If the download still fails, rerun the failed job.
- Do not publish by hand. Do not move a tag. Fix the cause, and let the workflow release the next version.

## Dry run

To build every target with no tag and no publish, run the Release workflow by hand with `dry_run` set:

```sh
gh workflow run Release -R yannelli/pi-rust -f dry_run=true
```
