#!/usr/bin/env node

const { execFileSync } = require("child_process");
const { join } = require("path");

const PLATFORMS = {
  "linux-x64": "@partiri/cli-linux-x64",
  "linux-arm64": "@partiri/cli-linux-arm64",
  "darwin-x64": "@partiri/cli-darwin-x64",
  "darwin-arm64": "@partiri/cli-darwin-arm64",
};

const key = `${process.platform}-${process.arch}`;
const pkg = PLATFORMS[key];

if (!pkg) {
  console.error(
    `partiri: unsupported platform ${process.platform}-${process.arch}\n` +
      `Supported: ${Object.keys(PLATFORMS).join(", ")}\n` +
      `Install from source instead: cargo install partiri-cli`
  );
  process.exit(1);
}

let binPath;
try {
  const pkgJson = require.resolve(`${pkg}/package.json`);
  binPath = join(pkgJson, "..", "bin", "partiri");
} catch {
  console.error(
    `partiri: could not find package ${pkg}\n` +
      `Try reinstalling: npm install -g @partiri/cli`
  );
  process.exit(1);
}

const result = require("child_process").spawnSync(binPath, process.argv.slice(2), {
  stdio: "inherit",
});

if (result.error) {
  console.error(`partiri: failed to run binary: ${result.error.message}`);
  process.exit(1);
}

process.exit(result.status ?? 1);
