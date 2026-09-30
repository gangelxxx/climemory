'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const { createHash } = require('node:crypto');
const { TARGETS, validateManifest, platformKey, install, download } = require('../bin/installer.cjs');

const bytes = Buffer.from('test executable bytes');
function manifest(version = '2.62.0', content = bytes) {
  return { version, tag: `v${version}`, repository: 'gangelxxx/climemory',
    assets: Object.fromEntries(Object.entries(TARGETS).map(([key, name]) => [key, {
      name, size: content.length, sha256: createHash('sha256').update(content).digest('hex'),
    }])) };
}
async function fixture(t) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'climemory test '));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  return root;
}
const key = process.platform === 'win32' ? 'win32-x64' : 'linux-x64';
function options(root, release = manifest(), content = bytes) {
  return { root, manifest: release, key, log: () => {},
    fetchAsset: (_, output) => fs.writeFile(output, content),
    execute: () => `CM ${release.version} — Read-only memory chat\ncm ingest-session\n` };
}

test('manifest binds every supported platform to a version, filename, size and hash', () => {
  validateManifest(manifest(), '2.62.0');
  assert.throws(() => validateManifest(manifest(), '2.63.0'));
  assert.throws(() => validateManifest(manifest('02.62.0'), '02.62.0'));
  for (const mutate of [m => delete m.assets['linux-x64'], m => m.assets['linux-x64'].name = '../cm',
    m => m.repository = 'https://evil/owner/repo', m => m.assets['win32-x64'].sha256 = 'bad',
    m => m.assets['darwin-arm64'].size = 0, m => m.tag = 'latest']) {
    const m = manifest(); mutate(m); assert.throws(() => validateManifest(m, m.version));
  }
  assert.throws(() => platformKey('win32', 'arm64'), /Unsupported/);
});

test('installs permanently, reuses identical bytes and preserves user files', async t => {
  const root = await fixture(t);
  await fs.mkdir(path.join(root, 'memory'));
  await fs.writeFile(path.join(root, 'memory/config.json'), 'user config');
  await fs.writeFile(path.join(root, '.gitignore'), '# user rules');
  const calls = [];
  const settings = options(root);
  settings.execute = (file, args) => { calls.push([file, args]); return `CM 2.62.0 — chat\ncm ingest-session`; };
  const target = await install(settings);
  assert.equal(calls[1][0], target);
  assert.deepEqual(calls[1][1], ['init']);
  assert.deepEqual(await fs.readFile(target), bytes);
  await install({ ...settings, fetchAsset: () => { throw new Error('Unexpected download'); } });
  assert.equal(await fs.readFile(path.join(root, 'memory/config.json'), 'utf8'), 'user config');
  assert.equal(await fs.readFile(path.join(root, '.gitignore'), 'utf8'), '# user rules\n/.climemory-install/\n');
  assert.deepEqual((await fs.readdir(path.join(root, '.climemory-install'))).sort(), ['npm.json']);
});

test('verified upgrades replace managed binaries; checksum failures preserve the old one', async t => {
  const root = await fixture(t);
  const target = await install(options(root));
  const next = Buffer.from('next binary');
  const newer = manifest('2.63.0', next);
  await assert.rejects(install(options(root, newer, Buffer.from('corrupt'))), /checksum/);
  assert.deepEqual(await fs.readFile(target), bytes);
  await install(options(root, newer, next));
  assert.deepEqual(await fs.readFile(target), next);
  assert.equal(JSON.parse(await fs.readFile(path.join(root, '.climemory-install/npm.json'))).version, '2.63.0');
});

test('unmanaged and modified binaries are never overwritten or executed', async t => {
  const root = await fixture(t);
  const target = path.join(root, key.startsWith('win32') ? 'cm.exe' : 'cm');
  await fs.writeFile(target, 'unrelated application');
  const settings = options(root);
  settings.execute = () => { throw new Error('Must not execute an unverified file'); };
  await assert.rejects(install(settings), /not managed/);
  assert.equal(await fs.readFile(target, 'utf8'), 'unrelated application');
});

test('wrong executable version and failed initialization propagate errors and release locks', async t => {
  const root = await fixture(t);
  await assert.rejects(install({ ...options(root), execute: () => 'CM 0.0.0 — wrong' }), /version\/interface/);
  await assert.rejects(install({ ...options(root), execute: (_, args) => {
    if (args[0] === 'init') throw new Error('MCP configuration conflict');
    return 'CM 2.62.0 — chat\ncm ingest-session';
  } }), /MCP configuration conflict/);
  assert.deepEqual(await fs.readdir(path.join(root, '.climemory-install')), ['npm.json']);
  await install(options(root));
});

test('a concurrent installer cannot enter the same project', async t => {
  const root = await fixture(t);
  await fs.mkdir(path.join(root, '.climemory-install/npm.lock'), { recursive: true });
  await assert.rejects(install(options(root)), /Another installation/);
  assert.ok((await fs.stat(path.join(root, '.climemory-install/npm.lock'))).isDirectory());
});

test('state directory symlink is rejected', async t => {
  const root = await fixture(t);
  const elsewhere = await fixture(t);
  await fs.symlink(elsewhere, path.join(root, '.climemory-install'), process.platform === 'win32' ? 'junction' : 'dir');
  await assert.rejects(install(options(root)), /real directory/);
  assert.deepEqual(await fs.readdir(elsewhere), []);
});

test('HTTPS downloads verify streams, reject redirects to HTTP, HTTP errors and tampering', async t => {
  const root = await fixture(t);
  const asset = manifest().assets[key];
  const output = name => path.join(root, name);
  await download('https://github.com/asset', output('good'), asset, async () => new Response(bytes));
  assert.deepEqual(await fs.readFile(output('good')), bytes);
  await assert.rejects(download('https://github.com/asset', output('bad'), asset,
    async () => new Response(Buffer.from('xxxxxxxxxxxxxxxxxxxxx'))), /checksum|size/);
  await assert.rejects(download('https://github.com/asset', output('redirect'), asset,
    async () => new Response(null, { status: 302, headers: { location: 'http://example.com/file' } })), /HTTPS/);
  await assert.rejects(download('https://github.com/asset', output('missing'), asset,
    async () => new Response('not found', { status: 404 })), /HTTP 404/);
  await assert.rejects(download('https://github.com/asset', output('large'), asset,
    async () => new Response(Buffer.alloc(100))), /exceeds/);
});

module.exports = { manifest };
