'use strict';
const fs = require('node:fs/promises');
const { check, REPO } = require('./release.cjs');
const { VERSION } = require('../bin/installer.cjs');

function inspect(pkg, metadata) {
  if (!metadata) return false;
  const repository = metadata.repository?.url;
  if (metadata.name !== pkg.name || repository !== `git+https://github.com/${REPO}.git`) {
    throw new Error('The npm name is registered to a different repository; refusing publication.');
  }
  const latest = metadata['dist-tags']?.latest;
  if (latest) {
    if (!VERSION.test(latest)) throw new Error(`Unexpected npm latest version: ${latest}`);
    const current = pkg.version.split('.').map(Number);
    const previous = latest.split('.').map(Number);
    for (let i = 0; i < 3; i++) {
      if (current[i] < previous[i]) throw new Error(`Refusing to move npm latest backwards from ${latest} to ${pkg.version}.`);
      if (current[i] > previous[i]) break;
    }
  }
  const existing = metadata.versions?.[pkg.version];
  if (existing && existing.repository?.url !== pkg.repository.url) {
    throw new Error('Existing npm version belongs to a different repository.');
  }
  return Boolean(existing);
}

async function main() {
  const pkg = await check();
  const response = await fetch(`https://registry.npmjs.org/${encodeURIComponent(pkg.name)}`, { signal: AbortSignal.timeout(30_000) });
  if (response.status !== 404 && !response.ok) throw new Error(`npm registry returned HTTP ${response.status}`);
  const metadata = response.status === 404 ? null : await response.json();
  const published = inspect(pkg, metadata);
  console.log(published ? `${pkg.name}@${pkg.version} already published; npm step will be skipped.` : `New npm version: ${pkg.version}`);
  if (process.env.GITHUB_OUTPUT) {
    await fs.appendFile(process.env.GITHUB_OUTPUT, `published=${published}\nversion=${pkg.version}\n`);
  }
}
if (require.main === module) main().catch(error => { console.error(error.message); process.exitCode = 1; });
module.exports = { inspect };
