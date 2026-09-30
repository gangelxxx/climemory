#!/usr/bin/env node
'use strict';

const path = require('node:path');
const fs = require('node:fs/promises');
const { install, validateManifest } = require('./installer.cjs');
const pkg = require('../package.json');

async function main(args) {
  if (args.length === 0 || (args.length === 1 && ['help', '--help', '-h'].includes(args[0]))) {
    console.log(`climemory ${pkg.version}\n\nUsage: npx @gangelxxx/climemory@latest init\n\nDownloads verified CM into the current project and runs cm init.\nRepeat with a newer package version to update CM; existing memory is preserved.\nUse ./cm (Unix) or .\\cm.exe (Windows) for memory commands after installation.`);
    return;
  }
  if (args.length === 1 && args[0] === '--version') {
    console.log(pkg.version);
    return;
  }
  if (args.length !== 1 || args[0] !== 'init') {
    throw new Error('Use npx @gangelxxx/climemory@latest init. Run the installed cm for other commands.');
  }
  let manifest;
  try {
    manifest = JSON.parse(await fs.readFile(path.join(__dirname, 'release.json'), 'utf8'));
  } catch (error) {
    if (error.code === 'ENOENT') {
      throw new Error('This checkout has no release manifest. Use a published @gangelxxx/climemory package or prepare a release first.');
    }
    throw error;
  }
  validateManifest(manifest, pkg.version);
  await install({ root: process.cwd(), manifest });
}

main(process.argv.slice(2)).catch(error => {
  console.error(`climemory: ${error.message}`);
  process.exitCode = 1;
});
