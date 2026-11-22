#!/usr/bin/env node
// Publishes a model catalog that upstream pi's `npm run generate:model-catalog` wrote into a
// checkout of the `model-catalog` branch. The layout is upstream's (`scripts/model-catalog-protocol.ts`
// in earendil-works/pi), keyed by pi-rust versions:
//
//   models/v1/index.json                                    revisions by minimum pi-rust version
//   models/v1/revisions/<revision>/models.json              chat models by provider and id
//   models/v1/revisions/<revision>/models.all.json          models of every type by provider
//   models/v1/revisions/<revision>/providers.json           sorted provider ids
//   models/v1/revisions/<revision>/providers/<id>.json      chat models of one provider by id
//   models/v1/revisions/<revision>/providers/<id>.all.json  models of every type of one provider
//
// pi-rust.com serves the branch at /api/models/providers/<id> (worker/model-catalog.ts there).
//
// Usage: publish-model-catalog.mjs --input <dir> --output <checkout> --minimum-version <x.y.z>
//        --upstream-commit <sha>

import { createHash } from 'node:crypto';
import { cpSync, existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

export const PREFIX = 'models/v1';
const REQUIRED_PROVIDERS = ['anthropic', 'openai', 'openrouter'];
const MINIMUM_MODEL_COUNT = 500;
/** Index entries kept; clients older than the oldest one fall back to their bundled models. */
export const KEEP_ENTRIES = 10;
const VERSION = /^v?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?(?:\+[0-9A-Za-z.-]+)?$/;
const PROVIDER_ID = /^[a-z0-9][a-z0-9._-]*$/;

function parseVersion(version) {
  const match = VERSION.exec(version);
  if (!match) throw new Error(`Invalid version: ${version}`);
  return [Number(match[1]), Number(match[2]), Number(match[3]), match[4] ?? ''];
}

/** Semver precedence without build metadata; a prerelease sorts before its release. */
export function compareVersions(left, right) {
  const a = parseVersion(left);
  const b = parseVersion(right);
  for (let index = 0; index < 3; index++) if (a[index] !== b[index]) return a[index] - b[index];
  if (a[3] === b[3]) return 0;
  if (!a[3]) return 1;
  if (!b[3]) return -1;
  return a[3] < b[3] ? -1 : 1;
}

/** Check the generator output and return its revision and provider ids. */
export function readBundle(input) {
  const allModelsBytes = readFileSync(join(input, 'models.all.json'));
  const allModels = JSON.parse(allModelsBytes.toString('utf8'));
  const providerIds = JSON.parse(readFileSync(join(input, 'providers.json'), 'utf8'));
  JSON.parse(readFileSync(join(input, 'models.json'), 'utf8'));
  if (!Array.isArray(providerIds) || !providerIds.every((id) => typeof id === 'string' && PROVIDER_ID.test(id))) {
    throw new Error('providers.json must be an array of provider ids');
  }
  for (const id of REQUIRED_PROVIDERS) if (!providerIds.includes(id)) throw new Error(`Missing provider: ${id}`);
  let count = 0;
  for (const id of providerIds) {
    for (const suffix of ['.json', '.all.json']) {
      const file = join(input, 'providers', `${id}${suffix}`);
      if (!existsSync(file)) throw new Error(`Missing provider file: providers/${id}${suffix}`);
    }
    count += Array.isArray(allModels[id]) ? allModels[id].length : 0;
  }
  if (count < MINIMUM_MODEL_COUNT) throw new Error(`Only ${count} models; expected at least ${MINIMUM_MODEL_COUNT}`);
  // Upstream's revision: the digest of models.all.json, so any model change makes a new revision.
  const revision = `sha256-${createHash('sha256').update(allModelsBytes).digest('hex')}`;
  return { revision, providerIds, count };
}

/** The index with `revision` published for clients at or above `minimumPiVersion`. */
export function updateIndex(index, { minimumPiVersion, revision, upstreamCommit, now }) {
  parseVersion(minimumPiVersion);
  const catalogs = (index?.catalogs ?? []).map((entry) => ({ ...entry }));
  const existing = catalogs.find((entry) => entry.minimumPiVersion === minimumPiVersion);
  if (existing && existing.revision === revision) {
    existing.upstreamCommit = upstreamCommit;
  } else if (existing) {
    Object.assign(existing, { revision, publishedAt: now, upstreamCommit });
  } else {
    catalogs.push({ minimumPiVersion, revision, publishedAt: now, upstreamCommit });
  }
  catalogs.sort((left, right) => compareVersions(left.minimumPiVersion, right.minimumPiVersion));
  const kept = catalogs.slice(-KEEP_ENTRIES);
  return { schemaVersion: 1, defaultRevision: kept[kept.length - 1].revision, catalogs: kept };
}

/** Copy the bundle into `output` and update its index. Returns the revision and whether files changed. */
export function publish({ input, output, minimumPiVersion, upstreamCommit, now = new Date().toISOString() }) {
  const { revision, count } = readBundle(input);
  const indexPath = join(output, PREFIX, 'index.json');
  const before = existsSync(indexPath) ? readFileSync(indexPath, 'utf8') : '';
  const index = updateIndex(before ? JSON.parse(before) : undefined, { minimumPiVersion, revision, upstreamCommit, now });
  const revisions = join(output, PREFIX, 'revisions');
  const target = join(revisions, revision);
  const added = !existsSync(target);
  if (added) {
    mkdirSync(target, { recursive: true });
    for (const name of ['models.json', 'models.all.json', 'providers.json']) cpSync(join(input, name), join(target, name));
    cpSync(join(input, 'providers'), join(target, 'providers'), { recursive: true });
  }
  const referenced = new Set(index.catalogs.map((entry) => entry.revision));
  let pruned = 0;
  for (const name of readdirSync(revisions)) {
    if (!referenced.has(name)) {
      rmSync(join(revisions, name), { recursive: true, force: true });
      pruned++;
    }
  }
  const after = `${JSON.stringify(index, null, 2)}\n`;
  if (after !== before) writeFileSync(indexPath, after);
  return { revision, count, changed: added || pruned > 0 || after !== before };
}

function parseArgs(args) {
  const names = { '--input': 'input', '--output': 'output', '--minimum-version': 'minimumPiVersion', '--upstream-commit': 'upstreamCommit' };
  const options = {};
  for (let index = 0; index < args.length; index += 2) {
    const name = names[args[index]];
    if (!name || args[index + 1] === undefined) throw new Error(`Unexpected argument: ${args[index]}`);
    options[name] = args[index + 1];
  }
  for (const [flag, name] of Object.entries(names)) if (!options[name]) throw new Error(`${flag} is required`);
  options.minimumPiVersion = options.minimumPiVersion.replace(/^v/, '');
  return options;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const result = publish(parseArgs(process.argv.slice(2)));
  console.log(`${result.changed ? 'Published' : 'Unchanged'} ${result.revision} (${result.count} models)`);
}
