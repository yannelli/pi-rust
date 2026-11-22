// node --test scripts/publish-model-catalog.test.mjs
import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { test } from 'node:test';

import { KEEP_ENTRIES, PREFIX, compareVersions, publish, updateIndex } from './publish-model-catalog.mjs';

function bundle(models = 600) {
  const dir = mkdtempSync(join(tmpdir(), 'catalog-input-'));
  const providers = ['anthropic', 'openai', 'openrouter'];
  const all = Object.fromEntries(providers.map((id, n) => [id, Array.from({ length: n === 0 ? models : 1 }, (_, i) => ({ id: `${id}-${i}`, type: 'chat' }))]));
  writeFileSync(join(dir, 'models.all.json'), JSON.stringify(all));
  writeFileSync(join(dir, 'models.json'), JSON.stringify(all));
  writeFileSync(join(dir, 'providers.json'), JSON.stringify(providers));
  mkdirSync(join(dir, 'providers'));
  for (const id of providers) {
    writeFileSync(join(dir, 'providers', `${id}.all.json`), JSON.stringify(all[id]));
    writeFileSync(join(dir, 'providers', `${id}.json`), JSON.stringify({}));
  }
  return dir;
}

test('versions compare by semver precedence', () => {
  assert.ok(compareVersions('0.12.10', '0.12.9') > 0);
  assert.ok(compareVersions('1.0.0-beta', '1.0.0') < 0);
  assert.equal(compareVersions('v1.2.3', '1.2.3+build'), 0);
  assert.throws(() => compareVersions('latest', '1.0.0'));
});

test('a new release adds an entry and becomes the default; the same revision keeps its date', () => {
  let index = updateIndex(undefined, { minimumPiVersion: '0.12.20', revision: 'sha256-a', upstreamCommit: 'c1', now: 't1' });
  index = updateIndex(index, { minimumPiVersion: '0.12.26', revision: 'sha256-b', upstreamCommit: 'c2', now: 't2' });
  assert.equal(index.defaultRevision, 'sha256-b');
  assert.deepEqual(index.catalogs.map((entry) => entry.minimumPiVersion), ['0.12.20', '0.12.26']);
  const same = updateIndex(index, { minimumPiVersion: '0.12.26', revision: 'sha256-b', upstreamCommit: 'c2', now: 't3' });
  assert.equal(same.catalogs[1].publishedAt, 't2');
  const fresh = updateIndex(index, { minimumPiVersion: '0.12.26', revision: 'sha256-c', upstreamCommit: 'c2', now: 't4' });
  assert.deepEqual(fresh.catalogs[1], { minimumPiVersion: '0.12.26', revision: 'sha256-c', publishedAt: 't4', upstreamCommit: 'c2' });
});

test('the index keeps the newest entries', () => {
  let index;
  for (let patch = 0; patch < KEEP_ENTRIES + 3; patch++) {
    index = updateIndex(index, { minimumPiVersion: `1.0.${patch}`, revision: `sha256-${patch}`, upstreamCommit: 'c', now: 't' });
  }
  assert.equal(index.catalogs.length, KEEP_ENTRIES);
  assert.equal(index.catalogs[0].minimumPiVersion, '1.0.3');
});

test('publish copies the revision, prunes unreferenced ones, and is idempotent', () => {
  const output = mkdtempSync(join(tmpdir(), 'catalog-output-'));
  const first = publish({ input: bundle(), output, minimumPiVersion: '0.12.26', upstreamCommit: 'c', now: 't1' });
  assert.ok(first.changed);
  assert.match(first.revision, /^sha256-[0-9a-f]{64}$/);
  const revisionDir = join(output, PREFIX, 'revisions', first.revision);
  assert.deepEqual(readdirSync(join(revisionDir, 'providers')).sort().slice(0, 2), ['anthropic.all.json', 'anthropic.json']);
  assert.equal(publish({ input: bundle(), output, minimumPiVersion: '0.12.26', upstreamCommit: 'c', now: 't2' }).changed, false);

  const second = publish({ input: bundle(601), output, minimumPiVersion: '0.12.26', upstreamCommit: 'c', now: 't3' });
  assert.ok(second.changed);
  assert.deepEqual(readdirSync(join(output, PREFIX, 'revisions')), [second.revision]);
  const index = JSON.parse(readFileSync(join(output, PREFIX, 'index.json'), 'utf8'));
  assert.equal(index.defaultRevision, second.revision);
  assert.equal(index.catalogs[0].publishedAt, 't3');
});

test('publish rejects incomplete bundles', () => {
  const output = mkdtempSync(join(tmpdir(), 'catalog-output-'));
  assert.throws(() => publish({ input: bundle(10), output, minimumPiVersion: '0.12.26', upstreamCommit: 'c' }), /at least 500/);
});
