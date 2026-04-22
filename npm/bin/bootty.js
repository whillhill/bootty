#!/usr/bin/env node

const { spawn } = require("node:child_process");
const { existsSync } = require("node:fs");
const { join } = require("node:path");

const binaryPath = join(__dirname, "bootty");

if (!existsSync(binaryPath)) {
  console.error("bootty native binary is missing.");
  console.error("Expected file:", binaryPath);
  process.exit(1);
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 1);
});

child.on("error", (error) => {
  console.error("Failed to launch bootty:", error.message);
  process.exit(1);
});
