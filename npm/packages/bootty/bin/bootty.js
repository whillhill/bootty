#!/usr/bin/env node

const { spawn } = require("node:child_process");

const PLATFORM_MAP = {
  "darwin-arm64": "bootty-darwin-arm64",
  "darwin-x64": "bootty-darwin-x64",
  "linux-x64": "bootty-linux-x64-gnu",
  "win32-x64": "bootty-win32-x64-msvc"
};

function normalizeArch(arch) {
  if (arch === "x64") return "x64";
  if (arch === "arm64") return "arm64";
  return arch;
}

const key = `${process.platform}-${normalizeArch(process.arch)}`;
const packageName = PLATFORM_MAP[key];

if (!packageName) {
  console.error(`[error] Unsupported platform: ${process.platform}/${process.arch}`);
  process.exit(1);
}

const binaryName = process.platform === "win32" ? "bootty.exe" : "bootty";
let binaryPath;

try {
  binaryPath = require.resolve(`${packageName}/bin/${binaryName}`);
} catch (error) {
  console.error(`[error] Platform package not found: ${packageName}. Please reinstall bootty.`);
  console.error(String(error && error.message ? error.message : error));
  process.exit(1);
}

const child = spawn(binaryPath, process.argv.slice(2), { stdio: "inherit" });

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});

child.on("error", (error) => {
  console.error(`[error] Failed to start bootty: ${error.message}`);
  process.exit(1);
});
