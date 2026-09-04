#!/usr/bin/env node
// Thin launcher for the prebuilt squeezed binary downloaded by install.js.
// If the postinstall hook was skipped (--ignore-scripts, offline install,
// pnpm with scripts disabled, …) it downloads the binary on first run.

'use strict';

const { spawn } = require('node:child_process');
const fs = require('node:fs');

const { binaryPath, install } = require('../install.js');

async function main() {
  if (!fs.existsSync(binaryPath)) {
    await install();
  }

  const child = spawn(binaryPath, process.argv.slice(2), { stdio: 'inherit' });
  // Forward termination signals so ctrl-C & co. reach the server cleanly.
  for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
    process.on(signal, () => child.kill(signal));
  }
  child.on('exit', (code, signal) => {
    if (signal) {
      process.kill(process.pid, signal);
      return;
    }
    process.exit(code ?? 1);
  });
}

main().catch((err) => {
  console.error(`squeezed: ${err.message}`);
  process.exit(1);
});
