# Model catalog

Created: 2026-11-22. Last updated: 2026-11-22.

pi-rust ships a bundled model list (`crates/pi-ai/models.json`). At run time it adds newer models from a catalog server: `GET <server>/api/models/providers/<id>?types=chat,image,classifier`, cached in `models-store.json` (`crates/pi-coding-agent/src/catalog/remote.rs`). The server is `https://pi-rust.com`. `PI_CATALOG_URL` replaces it. Upstream pi uses `pi.dev`.

## Why pi-rust has its own catalog

A catalog revision can need client code. Upstream publishes each revision for released pi versions at or above a minimum version, and pi.dev picks the revision from the version in the `User-Agent`. pi-rust versions are not pi versions, and pi-rust ports upstream code later. So pi-rust publishes catalogs that it generates at the upstream commit its own release ports.

## How a catalog is published

The [Model catalog workflow](../.github/workflows/model-catalog.yml) runs every 6 hours, after each Release workflow, and by hand.

1. It finds the latest release tag and its `syncedCommit` in `docs/parity/upstream.json`. The minimum version is the oldest consecutive release with the same `syncedCommit`.
2. A job with read-only permissions and no credentials checks out upstream at that commit and runs `npm run generate:model-catalog` and `npm run check:model-catalog`. The generator reads live sources such as models.dev, so prices and new models update between releases.
3. A second job runs `scripts/publish-model-catalog.mjs`. It copies the output into the `model-catalog` branch in upstream's layout (`models/v1/index.json` and `models/v1/revisions/<sha256>/...`), records the revision for the minimum version, keeps the 10 newest entries, and removes revisions no entry uses. It force-pushes the branch as one commit, so the branch adds no history to clones.

Test the script with `node --test scripts/publish-model-catalog.test.mjs`.

## How pi-rust.com serves it

The pi-rust.com Worker (`worker/model-catalog.ts` in yannelli/pi-rust.com) reads the branch from GitHub:

- It takes the version from `pi/<version>` or `pi-rust/<version>` in the `User-Agent`, or from `?pi-version=`. It serves the revision with the highest minimum version at or below it. Without a version it serves the newest revision.
- `?types=` returns `providers/<id>.all.json` (every model type). Without it the response is `providers/<id>.json` (chat models only), as upstream does.
- `Last-Modified` is the time the revision was published, so the client uses it only when it is newer than the bundled models. `ETag` names the revision; `If-None-Match` gets `304`.
- An unknown provider, or a client older than every entry, gets `404`. The client then uses its bundled models.
