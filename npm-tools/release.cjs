'use strict';

const fs = require('node:fs/promises');
const path = require('node:path');
const { TARGETS, VERSION, MAX_BYTES, validateManifest, hash } = require('../bin/installer.cjs');
const ROOT = path.resolve(__dirname, '..');
const REPO = 'gangelxxx/climemory';

async function check(root = ROOT) {
  const pkg = JSON.parse(await fs.readFile(path.join(root, 'package.json'), 'utf8'));
  const cargo = await fs.readFile(path.join(root, 'Cargo.toml'), 'utf8');
  const section = cargo.match(/^\[package\]\s*\r?\n([\s\S]*?)(?=^\[|$(?![\s\S]))/m)?.[1];
  const version = section?.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  if (pkg.name !== 'climemory' || !VERSION.test(pkg.version) || version !== pkg.version) {
    throw new Error('package.json (climemory) and Cargo.toml must have the same stable x.y.z version.');
  }
  if (pkg.repository?.url !== `git+https://github.com/${REPO}.git`) throw new Error('Unexpected package repository.');
  return pkg;
}

async function prepare(assetsDirectory, root = ROOT) {
  const pkg = await check(root);
  const manifest = { version: pkg.version, repository: REPO, tag: `v${pkg.version}`, assets: {} };
  const checksums = [];
  for (const [key, name] of Object.entries(TARGETS)) {
    const file = path.join(assetsDirectory, name);
    const info = await fs.lstat(file);
    if (!info.isFile() || info.isSymbolicLink() || info.size <= 0 || info.size > MAX_BYTES) {
      throw new Error(`Invalid release asset: ${name}`);
    }
    const sha256 = await hash(file);
    manifest.assets[key] = { name, sha256, size: info.size };
    checksums.push(`${sha256}  ${name}`);
  }
  validateManifest(manifest, pkg.version);
  await fs.mkdir(path.join(root, 'bin'), { recursive: true });
  await fs.writeFile(path.join(root, 'bin/release.json'), JSON.stringify(manifest, null, 2) + '\n');
  await fs.writeFile(path.join(assetsDirectory, 'SHA256SUMS'), checksums.join('\n') + '\n');
  return manifest;
}

async function verifyPackage(root = ROOT) {
  const pkg = await check(root);
  let manifest;
  try { manifest = JSON.parse(await fs.readFile(path.join(root, 'bin/release.json'), 'utf8')); }
  catch { throw new Error('Missing release manifest. Run the release workflow; do not publish an unprepared checkout.'); }
  validateManifest(manifest, pkg.version);
  if (manifest.repository !== REPO) throw new Error('Release manifest uses a different repository.');
  return manifest;
}

async function main([command, directory]) {
  if (command === 'check' && !directory) console.log((await check()).version);
  else if (command === 'prepare' && directory) await prepare(path.resolve(directory));
  else if (command === 'verify-package' && !directory) await verifyPackage();
  else throw new Error('Usage: node npm-tools/release.cjs check | prepare <assets-directory> | verify-package');
}
if (require.main === module) main(process.argv.slice(2)).catch(error => { console.error(error.message); process.exitCode = 1; });
module.exports = { check, prepare, verifyPackage, REPO };
