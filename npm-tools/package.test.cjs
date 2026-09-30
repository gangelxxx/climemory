'use strict';
const { test } = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const { execFileSync } = require('node:child_process');
const { TARGETS } = require('../bin/installer.cjs');
const { prepare } = require('./release.cjs');

test('packed npm artifact includes manifest, excludes private files and runs through npm exec', async t => {
  // npm passes its CLI path to lifecycle scripts on every supported platform.
  assert.ok(process.env.npm_execpath, 'Run this integration test through npm test.');
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'climemory package '));
  t.after(() => fs.rm(root, { recursive: true, force: true }));
  const source = path.resolve(__dirname, '..');
  for (const name of ['package.json', 'Cargo.toml', 'README.md']) {
    await fs.copyFile(path.join(source, name), path.join(root, name));
  }
  await fs.cp(path.join(source, 'bin'), path.join(root, 'bin'), { recursive: true });
  await fs.mkdir(path.join(root, 'assets'));
  for (const name of Object.values(TARGETS)) await fs.writeFile(path.join(root, 'assets', name), `fixture ${name}`);
  await prepare(path.join(root, 'assets'), root);
  await fs.mkdir(path.join(root, 'memory'));
  await fs.writeFile(path.join(root, 'memory/private.txt'), 'must never ship');
  const call = (args, cwd = root) => execFileSync(process.execPath, [process.env.npm_execpath, ...args], {
    cwd, encoding: 'utf8', windowsHide: true, timeout: 60_000,
    env: { ...process.env, npm_config_cache: path.join(root, 'cache'), npm_config_update_notifier: 'false',
      npm_config_audit: 'false', npm_config_fund: 'false' },
  });
  const [packed] = JSON.parse(call(['pack', '--json', '--ignore-scripts']));
  assert.deepEqual(packed.files.map(file => file.path).sort(),
    ['README.md', 'bin/climemory.cjs', 'bin/installer.cjs', 'bin/release.json', 'package.json'].sort());
  const consumer = path.join(root, 'consumer');
  await fs.mkdir(consumer);
  const output = call(['exec', '--yes', '--offline', `--package=${path.join(root, packed.filename)}`, '--', 'climemory', '--help'], consumer);
  assert.match(output, /Usage: npx climemory@latest init/);
  assert.deepEqual(await fs.readdir(consumer), []);
});
