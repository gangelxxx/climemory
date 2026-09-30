'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const os = require('node:os');
const path = require('node:path');
const { check, prepare, verifyPackage } = require('./release.cjs');
const { TARGETS } = require('../bin/installer.cjs');
const { inspect } = require('./registry.cjs');
const { publishAssets } = require('./github-release.cjs');

async function fixture(t) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'climemory release '));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  await fs.copyFile(path.join(__dirname, '../package.json'), path.join(root, 'package.json'));
  await fs.copyFile(path.join(__dirname, '../Cargo.toml'), path.join(root, 'Cargo.toml'));
  await fs.mkdir(path.join(root, 'assets'));
  return root;
}
test('release requires matching Rust/npm versions and a complete artifact set', async t => {
  const root = await fixture(t);
  const pkg = await check(root);
  await assert.rejects(verifyPackage(root), /Missing release manifest/);
  await assert.rejects(prepare(path.join(root, 'assets'), root), /ENOENT/);
  for (const name of Object.values(TARGETS)) await fs.writeFile(path.join(root, 'assets', name), `binary ${name}`);
  const release = await prepare(path.join(root, 'assets'), root);
  assert.equal(release.version, pkg.version);
  assert.equal(Object.keys((await verifyPackage(root)).assets).length, 5);
  assert.equal((await fs.readFile(path.join(root, 'assets/SHA256SUMS'), 'utf8')).trim().split('\n').length, 5);
  pkg.version = '999.0.0';
  await fs.writeFile(path.join(root, 'package.json'), JSON.stringify(pkg));
  await assert.rejects(check(root), /same stable/);
});

test('registry check distinguishes retries from name collisions and downgrades', () => {
  const pkg = require('../package.json');
  assert.equal(inspect(pkg, null), false);
  const metadata = { name: pkg.name, repository: pkg.repository, 'dist-tags': { latest: pkg.version },
    versions: { [pkg.version]: pkg } };
  assert.equal(inspect(pkg, metadata), true);
  assert.equal(inspect({ ...pkg, version: '3.0.0' }, metadata), false);
  assert.throws(() => inspect({ ...pkg, version: '1.0.0' }, metadata), /backwards/);
  assert.throws(() => inspect(pkg, { ...metadata, repository: { url: 'https://example.com/other' } }), /different repository/);
  assert.throws(() => inspect(pkg, { ...metadata, 'dist-tags': { latest: '3.0.0-beta.1' } }), /Unexpected/);
});

test('release retries reuse public bytes and never reupload them', async t => {
  const root = await fixture(t);
  const dir = path.join(root, 'assets');
  const pkg = await check(root);
  const sha = 'a'.repeat(40);
  const calls = [];
  await publishAssets({ pkg, sha, dir,
    apiCall: async endpoint => {
      calls.push(endpoint);
      if (endpoint.startsWith('git/ref')) return { object: { type: 'commit', sha } };
      if (endpoint.startsWith('releases/tags')) return { id: 1, target_commitish: sha, draft: false };
      throw new Error(`Unexpected mutation: ${endpoint}`);
    },
    ghRun: async args => {
      assert.equal(args[1], 'download');
      for (const name of Object.values(TARGETS)) await fs.writeFile(path.join(dir, name), `published ${name}`);
      await prepare(dir, root);
    },
    prepareAssets: () => prepare(dir, root),
  });
  assert.equal(calls.length, 2);
});

test('release recovers an unpublished draft before making it public', async t => {
  const root = await fixture(t);
  const dir = path.join(root, 'assets');
  for (const name of Object.values(TARGETS)) await fs.writeFile(path.join(dir, name), `binary ${name}`);
  const pkg = await check(root);
  const sha = 'a'.repeat(40);
  const events = [];
  await publishAssets({ pkg, sha, dir,
    apiCall: async (endpoint, opts) => {
      if (endpoint.startsWith('git/ref') || endpoint.startsWith('releases/tags')) return null;
      if (endpoint.startsWith('releases?')) return [{ id: 1, tag_name: `v${pkg.version}`, target_commitish: sha, draft: true }];
      assert.equal(endpoint, 'releases/1');
      assert.equal(opts.method, 'PATCH');
      assert.equal(JSON.parse(opts.body).draft, false);
      events.push('publish');
      return {};
    },
    ghRun: async args => { events.push(args[1]); },
    prepareAssets: () => prepare(dir, root),
  });
  assert.deepEqual(events, ['upload', 'publish', 'download']);
});

test('release rejects a tag belonging to another commit', async t => {
  const root = await fixture(t);
  await assert.rejects(publishAssets({ pkg: await check(root), sha: 'a'.repeat(40), dir: path.join(root, 'assets'),
    apiCall: async () => ({ object: { type: 'commit', sha: 'b'.repeat(40) } }),
  }), /different commit/);
});
