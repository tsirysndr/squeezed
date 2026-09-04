// Downloads the prebuilt `squeezed` binary for this platform from GitHub
// Releases. Runs as the package's postinstall hook; the bin shim
// (bin/squeezed.js) also calls install() as a fallback when the hook was
// skipped (e.g. `npm install --ignore-scripts`).
//
// Release assets are named `squeezed-v<version>-<os>-<arch>.tar.gz` with a
// matching `.tar.gz.sha256`, and contain the binary at the tarball root —
// see .github/workflows/release.yml in the repo.

'use strict';

const { execFileSync } = require('node:child_process');
const crypto = require('node:crypto');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');

const pkg = require('./package.json');

const REPO = 'tsirysndr/squeezed';
const TAG = `v${pkg.version}`;

const binaryPath = path.join(__dirname, 'vendor', 'squeezed');

function releaseLabel() {
  const platform = { darwin: 'darwin', linux: 'linux', freebsd: 'freebsd' }[process.platform];
  const arch = { x64: 'amd64', arm64: 'arm64' }[process.arch];
  if (!platform || !arch) {
    throw new Error(
      `squeezed has no prebuilt binary for ${process.platform}-${process.arch}. ` +
        'Build from source instead: https://github.com/tsirysndr/squeezed#build-from-source'
    );
  }
  if (platform === 'freebsd' && arch !== 'amd64') {
    throw new Error(
      'squeezed publishes FreeBSD binaries for amd64 only. ' +
        'Build from source instead: https://github.com/tsirysndr/squeezed#build-from-source'
    );
  }
  return `${platform}-${arch}`;
}

async function fetchBuffer(url) {
  const res = await fetch(url, { redirect: 'follow' });
  if (!res.ok) {
    throw new Error(`download failed: ${url} → HTTP ${res.status}`);
  }
  return Buffer.from(await res.arrayBuffer());
}

async function install() {
  const name = `squeezed-${TAG}-${releaseLabel()}`;
  const base = `https://github.com/${REPO}/releases/download/${TAG}`;
  const url = `${base}/${name}.tar.gz`;

  console.log(`squeezed: downloading ${url}`);
  const tarball = await fetchBuffer(url);

  // Verify against the published checksum; tolerate the .sha256 asset being
  // absent (it never is for current releases) but never a mismatch.
  try {
    const sha = (await fetchBuffer(`${url}.sha256`)).toString('utf8').trim().split(/\s+/)[0];
    const actual = crypto.createHash('sha256').update(tarball).digest('hex');
    if (sha !== actual) {
      throw new Error(`checksum mismatch for ${name}.tar.gz: expected ${sha}, got ${actual}`);
    }
  } catch (err) {
    if (String(err).includes('checksum mismatch')) throw err;
    console.warn(`squeezed: could not verify checksum (${err.message}); continuing`);
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'squeezed-npm-'));
  try {
    const archive = path.join(tmp, `${name}.tar.gz`);
    fs.writeFileSync(archive, tarball);
    execFileSync('tar', ['-xzf', archive, '-C', tmp]);

    const binary = path.join(tmp, 'squeezed');
    if (!fs.existsSync(binary)) {
      throw new Error(`tarball ${name}.tar.gz did not contain a squeezed binary`);
    }
    fs.mkdirSync(path.dirname(binaryPath), { recursive: true });
    fs.copyFileSync(binary, binaryPath);
    fs.chmodSync(binaryPath, 0o755);
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }

  console.log(`squeezed: installed ${binaryPath}`);
}

module.exports = { binaryPath, install };

if (require.main === module) {
  install().catch((err) => {
    console.error(`squeezed: install failed: ${err.message}`);
    process.exit(1);
  });
}
