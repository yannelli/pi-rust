---
name: ship-release
description: Get a pi-rust change from a branch to a published GitHub release. Use when asked to ship, release, or publish pi-rust, or to check why a release did not appear.
---

# Ship a release

Full playbook: `docs/agents/release.md`.

1. Run the repository checks locally. Open a PR to `main` with Conventional Commits; they set the version bump.
2. Wait for CI on Linux, macOS, and Windows. On a failure, use the `ci-triage` skill.
3. Merge only when the user has asked you to. The Release workflow then runs after CI passes on `main`.
4. Confirm the release: `gh release view <tag> -R yannelli/pi-rust --json assets --jq '.assets[].name'` lists 9 assets (7 platform archives, `SHA256SUMS`, `pi-env-daemons-<tag>.tar.gz`).
5. If a build or daemon job failed outside the code, rerun only the failed jobs. Never publish or tag by hand, and never edit versions or `CHANGELOG.md`.
