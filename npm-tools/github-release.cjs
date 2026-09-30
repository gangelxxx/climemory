'use strict';

const fs = require('node:fs/promises');
const path = require('node:path');
const { execFileSync } = require('node:child_process');
const { check, prepare, REPO } = require('./release.cjs');
const { TARGETS } = require('../bin/installer.cjs');

async function api(endpoint, options = {}) {
  const response = await fetch(`https://api.github.com/repos/${REPO}/${endpoint}`, {
    ...options, signal: AbortSignal.timeout(60_000), headers: {
      Authorization: `Bearer ${process.env.GH_TOKEN}`, Accept: 'application/vnd.github+json',
      'X-GitHub-Api-Version': '2022-11-28', 'Content-Type': 'application/json',
    },
  });
  if (response.status === 404 && options.method === undefined) return null;
  if (!response.ok) throw new Error(`GitHub ${endpoint}: HTTP ${response.status}: ${await response.text()}`);
  return response.json();
}
function gh(args) {
  execFileSync('gh', args, { stdio: 'inherit', windowsHide: true, timeout: 300_000 });
}
async function publishAssets({ pkg, sha, dir, apiCall = api, ghRun = gh, prepareAssets = prepare }) {
  const tag = `v${pkg.version}`;
  const ref = await apiCall(`git/ref/tags/${tag}`);
  if (ref && (ref.object.type !== 'commit' || ref.object.sha !== sha)) {
    throw new Error(`${tag} already points at a different commit or is not a lightweight release tag.`);
  }
  let release = await apiCall(`releases/tags/${tag}`);
  // The by-tag endpoint may not return an unpublished draft. Resume that draft
  // instead of creating another release after an interrupted upload.
  if (!release) {
    for (let page = 1; ; page++) {
      const releases = await apiCall(`releases?per_page=100&page=${page}`);
      if (!Array.isArray(releases)) throw new Error('Invalid GitHub release list.');
      release = releases.find(item => item.tag_name === tag);
      if (release || releases.length < 100) break;
    }
  }
  if (release && release.target_commitish !== sha) throw new Error('Existing release belongs to a different commit.');
  if (!release) {
    release = await apiCall('releases', { method: 'POST', body: JSON.stringify({ tag_name: tag,
      target_commitish: sha, name: tag, draft: true, generate_release_notes: true }) });
  }
  if (release.draft) {
    await prepareAssets(dir);
    await ghRun(['release', 'upload', tag, ...[...Object.values(TARGETS), 'SHA256SUMS'].map(name => path.join(dir, name)),
      '--repo', REPO, '--clobber']); // Only unpublished drafts can be replaced.
    await apiCall(`releases/${release.id}`, { method: 'PATCH', body: JSON.stringify({ draft: false, make_latest: 'true' }) });
  }
  // A retry uses already published assets, never replaces binaries used by npm clients.
  await ghRun(['release', 'download', tag, '--repo', REPO, '--dir', dir, '--clobber',
    ...[...Object.values(TARGETS), 'SHA256SUMS'].flatMap(name => ['--pattern', name])]);
  const original = await fs.readFile(path.join(dir, 'SHA256SUMS'), 'utf8');
  await prepareAssets(dir);
  if (original !== await fs.readFile(path.join(dir, 'SHA256SUMS'), 'utf8')) {
    throw new Error('Published release does not match its SHA256SUMS.');
  }
  console.log(`Release ready: https://github.com/${REPO}/releases/tag/${tag}`);
}
async function main() {
  const pkg = await check();
  const sha = process.env.GITHUB_SHA;
  if (process.env.GITHUB_REPOSITORY !== REPO || !/^[a-f0-9]{40}$/.test(sha || '') || !process.env.GH_TOKEN) {
    throw new Error('Release requires the configured GitHub repository, commit SHA and GH_TOKEN.');
  }
  await publishAssets({ pkg, sha, dir: path.resolve(process.argv[2] || 'release-assets') });
}
if (require.main === module) main().catch(error => { console.error(error.message); process.exitCode = 1; });
module.exports = { publishAssets };
