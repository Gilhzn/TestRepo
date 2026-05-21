// Manifest validation — runs in plain Node, doesn't require the vscode host.
// We inspect package.json + walk the extension's source to confirm every
// declared command has a registerCommand call.

import * as fs from "node:fs";
import * as path from "node:path";
import manifest from "../package.json";

const declared = manifest.contributes.commands.map((c) => c.command);
const expected = [
  "mosaic.commit",
  "mosaic.log",
  "mosaic.branches",
  "mosaic.status",
  "mosaic.openDashboard",
];
for (const cmd of expected) {
  if (!declared.includes(cmd)) {
    throw new Error(`package.json missing command: ${cmd}`);
  }
}

const extensionSrc = fs.readFileSync(
  path.join(__dirname, "..", "..", "src", "extension.ts"),
  "utf-8",
);
for (const cmd of declared) {
  const pattern = `registerCommand("${cmd}"`;
  if (!extensionSrc.includes(pattern)) {
    throw new Error(
      `extension.ts is missing registerCommand for "${cmd}"`,
    );
  }
}

// activationEvents must include onStartupFinished so the LSP starts even
// without a Mosaic file being opened first.
if (!manifest.activationEvents.includes("onStartupFinished")) {
  throw new Error("manifest.activationEvents must include onStartupFinished");
}

// The LSP client dep must be declared.
if (!("vscode-languageclient" in manifest.dependencies)) {
  throw new Error("manifest.dependencies must include vscode-languageclient");
}

console.log(`mosaic-vscode: ${declared.length} commands declared & wired`);
