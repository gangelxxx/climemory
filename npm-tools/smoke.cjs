'use strict';

// Exercises the real CM executable without model calls or production memory.
const fs = require('node:fs/promises');
const path = require('node:path');
const os = require('node:os');
const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const { install, hash, TARGETS, platformKey } = require('../bin/installer.cjs');
const { REPO } = require('./release.cjs');

async function smoke(binary) {
  const root = await fs.mkdtemp(path.join(os.tmpdir(), 'climemory smoke '));
  try {
    const version = require('../package.json').version;
    const info = { size: (await fs.stat(binary)).size, sha256: await hash(binary) };
    const manifest = { version, repository: REPO, tag: `v${version}`,
      assets: Object.fromEntries(Object.entries(TARGETS).map(([key, name]) => [key, { ...info, name }])) };
    const target = await install({ root, manifest, fetchAsset: (_, output) => fs.copyFile(binary, output) });
    const configPath = path.join(root, 'memory/config.json');
    const config = await fs.readFile(configPath, 'utf8');
    await fs.writeFile(path.join(root, 'memory/docs/keep.md'), 'Keep user requirements.\n');
    await fs.appendFile(path.join(root, 'AGENTS.md'), '\nUser-owned instructions.\n');
    const mcp = await fs.readFile(path.join(root, '.codex/config.toml'), 'utf8');
    assert.ok(mcp.includes('--mcp'));
    // TOML may use literal or escaped basic strings on Windows.
    assert.ok(mcp.includes(target) || mcp.includes(target.replaceAll('\\', '\\\\')));
    await install({ root, manifest, fetchAsset: () => { throw new Error('Same-version install must not download.'); } });
    assert.equal(await fs.readFile(configPath, 'utf8'), config);
    assert.equal(await fs.readFile(path.join(root, 'memory/docs/keep.md'), 'utf8'), 'Keep user requirements.\n');
    assert.ok((await fs.readFile(path.join(root, 'AGENTS.md'), 'utf8')).includes('User-owned instructions.'));
    const help = spawnSync(target, ['help'], { cwd: root, encoding: 'utf8', windowsHide: true });
    assert.equal(help.status, 0, help.stderr);
    assert.ok(help.stdout.startsWith(`CM ${version} — `));
    assert.equal((await fs.readFile(path.join(root, '.gitignore'), 'utf8')).split('/.climemory-install/').length, 2);
    console.log(`PASS: real CM ${platformKey()} install, MCP permanent path, repeated init and memory preservation`);
  } finally {
    // Only the exact mkdtemp result is removed, never an input project directory.
    await fs.rm(root, { recursive: true, force: true });
  }
}
smoke(path.resolve(process.argv[2] || 'target/release/' + (process.platform === 'win32' ? 'cm.exe' : 'cm')))
  .catch(error => { console.error(error); process.exitCode = 1; });
