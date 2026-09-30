'use strict';

const fs = require('node:fs/promises');
const { createReadStream, createWriteStream } = require('node:fs');
const { createHash, randomUUID } = require('node:crypto');
const path = require('node:path');
const { spawnSync } = require('node:child_process');
const { Readable, Transform } = require('node:stream');
const { pipeline } = require('node:stream/promises');

const TARGETS = Object.freeze({
  'win32-x64': 'cm-windows-x64.exe',
  'linux-x64': 'cm-linux-x64',
  'linux-arm64': 'cm-linux-arm64',
  'darwin-x64': 'cm-macos-x64',
  'darwin-arm64': 'cm-macos-arm64',
});
const VERSION = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const REPOSITORY = /^[A-Za-z0-9][A-Za-z0-9-]*\/[A-Za-z0-9][A-Za-z0-9._-]*$/;
const MAX_BYTES = 256 * 1024 * 1024;

function validateManifest(manifest, version) {
  if (!manifest || manifest.version !== version || !VERSION.test(version)
      || !REPOSITORY.test(manifest.repository) || manifest.tag !== `v${version}`) {
    throw new Error('Invalid release version or GitHub repository in bundled manifest.');
  }
  for (const [key, name] of Object.entries(TARGETS)) {
    const asset = manifest.assets?.[key];
    if (asset?.name !== name || !/^[a-f0-9]{64}$/.test(asset.sha256)
        || !Number.isSafeInteger(asset.size) || asset.size <= 0 || asset.size > MAX_BYTES) {
      throw new Error(`Invalid or missing release asset: ${key}`);
    }
  }
}

function platformKey(platform = process.platform, arch = process.arch) {
  const key = `${platform}-${arch}`;
  if (!TARGETS[key]) throw new Error(`Unsupported platform ${key}. Build cm from source for this platform.`);
  if (platform === 'linux' && platform === process.platform
      && !process.report.getReport().header.glibcVersionRuntime) {
    throw new Error('Linux releases require glibc (Ubuntu 22.04 or newer); musl/Alpine is not supported.');
  }
  return key;
}

async function stat(file) {
  try { return await fs.lstat(file); }
  catch (error) { if (error.code === 'ENOENT') return null; throw error; }
}

async function hash(file) {
  const digest = createHash('sha256');
  for await (const bytes of createReadStream(file)) digest.update(bytes);
  return digest.digest('hex');
}

async function download(url, destination, asset, fetcher = fetch) {
  const controller = new AbortController();
  const timeout = setTimeout(() => controller.abort(), 120_000);
  try {
    let response;
    for (let redirects = 0; redirects <= 5; redirects++) {
      const parsed = new URL(url);
      if (parsed.protocol !== 'https:' || parsed.username || parsed.password) {
        throw new Error('Release downloads must use credential-free HTTPS.');
      }
      response = await fetcher(url, { redirect: 'manual', signal: controller.signal });
      if (![301, 302, 303, 307, 308].includes(response.status)) break;
      const location = response.headers.get('location');
      await response.body?.cancel();
      if (!location || redirects === 5) throw new Error('Invalid or excessive release redirects.');
      url = new URL(location, url).href;
    }
    if (!response.ok || !response.body) {
      await response.body?.cancel();
      throw new Error(`Release download failed (HTTP ${response.status}). Check that this version has a public GitHub Release.`);
    }
    let size = 0;
    const digest = createHash('sha256');
    const verify = new Transform({
      transform(chunk, encoding, callback) {
        size += chunk.length;
        if (size > asset.size || size > MAX_BYTES) return callback(new Error('Release download exceeds expected size.'));
        digest.update(chunk);
        callback(null, chunk);
      },
    });
    await pipeline(Readable.fromWeb(response.body), verify,
      createWriteStream(destination, { flags: 'wx', mode: 0o700 }), { signal: controller.signal });
    if (size !== asset.size || digest.digest('hex') !== asset.sha256) {
      throw new Error('Release checksum/size mismatch; existing CM was not replaced.');
    }
  } finally {
    clearTimeout(timeout);
  }
}

function run(file, args, options) {
  const result = spawnSync(file, args, { ...options, windowsHide: true, shell: false });
  if (result.error) throw result.error;
  if (result.signal || result.status !== 0) {
    throw new Error(`CM ${args.join(' ')} failed (${result.signal || result.status}). ${result.stderr || ''}`.trim());
  }
  return result.stdout || '';
}

async function install({ root, manifest, key = platformKey(), fetchAsset = download, execute = run, log = console.error }) {
  validateManifest(manifest, manifest.version);
  const asset = manifest.assets[key];
  if (!TARGETS[key] || !asset) throw new Error(`Unsupported platform ${key}`);
  root = await fs.realpath(root);
  const target = path.join(root, key.startsWith('win32-') ? 'cm.exe' : 'cm');
  const state = path.join(root, '.climemory-install');
  await fs.mkdir(state, { recursive: true });
  if (!(await fs.lstat(state)).isDirectory() || (await fs.lstat(state)).isSymbolicLink()) {
    throw new Error('Installation state must be a real directory, not a symlink.');
  }
  const lock = path.join(state, 'npm.lock');
  try { await fs.mkdir(lock); }
  catch (error) {
    if (error.code !== 'EEXIST') throw error;
    throw new Error(`Another installation may be active. If it was interrupted, remove ${lock} after checking no installer is running.`);
  }
  const temporary = path.join(state, `npm-${randomUUID()}${key.startsWith('win32-') ? '.exe' : '.tmp'}`);
  const receiptPath = path.join(state, 'npm.json');
  const receiptTemp = path.join(state, `receipt-${randomUUID()}.tmp`);
  try {
    const existing = await stat(target);
    if (existing && (!existing.isFile() || existing.isSymbolicLink())) {
      throw new Error(`Refusing to replace a non-regular file: ${target}`);
    }
    const previousHash = existing ? await hash(target) : null;
    if (previousHash && previousHash !== asset.sha256) {
      let receipt;
      try { receipt = JSON.parse(await fs.readFile(receiptPath, 'utf8')); }
      catch (error) { if (error.code !== 'ENOENT' && !(error instanceof SyntaxError)) throw error; }
      if (receipt?.sha256 !== previousHash || receipt?.repository !== manifest.repository || receipt?.key !== key) {
        throw new Error(`Existing ${path.basename(target)} is not managed by this installer or was modified. Move it aside explicitly before installing.`);
      }
    }
    if (previousHash !== asset.sha256) {
      const url = `https://github.com/${manifest.repository}/releases/download/${manifest.tag}/${asset.name}`;
      log(`Downloading CM ${manifest.version} (${key})...`);
      await fetchAsset(url, temporary, asset);
      // Recheck even when a caller supplies a download implementation (offline tests).
      if ((await fs.stat(temporary)).size !== asset.size || await hash(temporary) !== asset.sha256) {
        throw new Error('Release checksum/size mismatch; existing CM was not replaced.');
      }
      await fs.chmod(temporary, 0o755);
      const help = execute(temporary, ['help'], { cwd: root, encoding: 'utf8', timeout: 15_000 });
      if (!help.startsWith(`CM ${manifest.version} — `) || !help.includes('cm ingest-session')) {
        throw new Error('Downloaded executable does not match the expected CM version/interface.');
      }
      // Same filesystem: atomic replacement, or an error if Windows has CM open.
      await fs.rename(temporary, target);
    }
    await fs.writeFile(receiptTemp, JSON.stringify({ version: manifest.version, repository: manifest.repository,
      key, sha256: asset.sha256 }, null, 2) + '\n', { flag: 'wx' });
    await fs.rename(receiptTemp, receiptPath);
    log(`Installed CM ${manifest.version}: ${target}`);
    // Invoke the permanent path so MCP never points at the npm cache or a temp file.
    execute(target, ['init'], { cwd: root, stdio: 'inherit', timeout: 60_000 });
    const ignore = path.join(root, '.gitignore');
    const ignoreStat = await stat(ignore);
    if (ignoreStat && (!ignoreStat.isFile() || ignoreStat.isSymbolicLink())) throw new Error('.gitignore must be a regular file.');
    const text = ignoreStat ? await fs.readFile(ignore, 'utf8') : '';
    if (!text.split(/\r?\n/).includes('/.climemory-install/')) {
      await fs.appendFile(ignore, `${text && !text.endsWith('\n') ? '\n' : ''}/.climemory-install/\n`);
    }
    log('Configure memory/config.json, then open/restart Codex in this project.');
    return target;
  } finally {
    await fs.rm(temporary, { force: true });
    await fs.rm(receiptTemp, { force: true });
    await fs.rmdir(lock);
  }
}

module.exports = { TARGETS, VERSION, REPOSITORY, MAX_BYTES, validateManifest, platformKey, hash, download, install };
