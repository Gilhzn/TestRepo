import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
  TransportKind,
} from "vscode-languageclient/node";

let client: LanguageClient | undefined;
let statusItem: vscode.StatusBarItem | undefined;

const OUTPUT_CHANNEL_NAME = "Mosaic";

function config(): vscode.WorkspaceConfiguration {
  return vscode.workspace.getConfiguration("mosaic");
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  const output = vscode.window.createOutputChannel(OUTPUT_CHANNEL_NAME);
  context.subscriptions.push(output);

  const lspPath = config().get<string>("lspPath", "mosaic-lsp");

  const serverOptions: ServerOptions = {
    run: { command: lspPath, transport: TransportKind.stdio },
    debug: { command: lspPath, transport: TransportKind.stdio },
  };

  const clientOptions: LanguageClientOptions = {
    // Track every text document; let the LSP server decide which ones it
    // cares about (anything inside a workspace folder with a .mosaic/ dir).
    documentSelector: [{ scheme: "file" }],
    synchronize: {
      fileEvents: vscode.workspace.createFileSystemWatcher(
        "**/.mosaic/**",
      ),
    },
    outputChannel: output,
  };

  client = new LanguageClient(
    "mosaic-lsp",
    "Mosaic LSP",
    serverOptions,
    clientOptions,
  );

  try {
    await client.start();
    output.appendLine(`mosaic-lsp started (binary: ${lspPath})`);
  } catch (err) {
    output.appendLine(`failed to start mosaic-lsp: ${err}`);
    vscode.window.showWarningMessage(
      `Mosaic: could not start mosaic-lsp at "${lspPath}". Set mosaic.lspPath to the binary location.`,
    );
  }

  statusItem = vscode.window.createStatusBarItem(
    vscode.StatusBarAlignment.Left,
    100,
  );
  statusItem.text = "$(git-commit) Mosaic";
  statusItem.tooltip = "Mosaic: click for repository status";
  statusItem.command = "mosaic.status";
  statusItem.show();
  context.subscriptions.push(statusItem);

  context.subscriptions.push(
    vscode.commands.registerCommand("mosaic.commit", () => commitCurrentFile(output)),
    vscode.commands.registerCommand("mosaic.log", () => showLog(output)),
    vscode.commands.registerCommand("mosaic.branches", () => showBranches(output)),
    vscode.commands.registerCommand("mosaic.status", () => showStatus(output)),
    vscode.commands.registerCommand("mosaic.openDashboard", () => openDashboard()),
  );
}

export async function deactivate(): Promise<void> {
  if (client) {
    await client.stop();
    client = undefined;
  }
}

async function commitCurrentFile(output: vscode.OutputChannel): Promise<void> {
  if (!client) {
    vscode.window.showErrorMessage("Mosaic LSP is not running");
    return;
  }
  const editor = vscode.window.activeTextEditor;
  if (!editor) {
    vscode.window.showWarningMessage("Mosaic: no active editor");
    return;
  }
  const intent = await vscode.window.showInputBox({
    prompt: "Intent for this change",
    placeHolder: "e.g. fix charge() rounding",
  });
  if (!intent) return;

  const branch = config().get<string>("defaultBranch", "main");
  const args = { uri: editor.document.uri.toString(), intent, branch };

  await editor.document.save();

  try {
    const result = (await client.sendRequest("workspace/executeCommand", {
      command: "mosaic.commit",
      arguments: [args],
    })) as { change: string; path: string; branch: string } | null;
    if (result) {
      output.appendLine(`committed ${result.change.slice(0, 12)} on ${result.branch} (${result.path})`);
      vscode.window.showInformationMessage(
        `Mosaic: committed ${result.change.slice(0, 12)}`,
      );
    }
  } catch (err) {
    vscode.window.showErrorMessage(`Mosaic commit failed: ${err}`);
  }
}

async function showLog(output: vscode.OutputChannel): Promise<void> {
  if (!client) return;
  try {
    const result = (await client.sendRequest("workspace/executeCommand", {
      command: "mosaic.log",
      arguments: [],
    })) as { branch: string; changes: string[] } | null;
    if (!result) return;
    output.show(true);
    output.appendLine(`--- log on ${result.branch} (${result.changes.length} change(s)) ---`);
    for (const id of result.changes) {
      output.appendLine(`  ${id.slice(0, 16)}`);
    }
  } catch (err) {
    vscode.window.showErrorMessage(`Mosaic log failed: ${err}`);
  }
}

async function showBranches(output: vscode.OutputChannel): Promise<void> {
  if (!client) return;
  try {
    const result = (await client.sendRequest("workspace/executeCommand", {
      command: "mosaic.branches",
      arguments: [],
    })) as { branches: string[] } | null;
    if (!result) return;
    if (result.branches.length === 0) {
      vscode.window.showInformationMessage("Mosaic: no branches yet");
      return;
    }
    output.show(true);
    output.appendLine("--- branches ---");
    for (const b of result.branches) {
      output.appendLine(`  ${b}`);
    }
  } catch (err) {
    vscode.window.showErrorMessage(`Mosaic branches failed: ${err}`);
  }
}

async function showStatus(output: vscode.OutputChannel): Promise<void> {
  if (!client) return;
  try {
    const result = (await client.sendRequest("workspace/executeCommand", {
      command: "mosaic.status",
      arguments: [],
    })) as {
      repo: string;
      identity: string;
      branches: string[];
      open_files: string[];
    } | null;
    if (!result) return;
    output.show(true);
    output.appendLine("--- mosaic status ---");
    output.appendLine(`repo:     ${result.repo}`);
    output.appendLine(`identity: ${result.identity}`);
    output.appendLine(`branches: ${result.branches.join(", ") || "(none)"}`);
    output.appendLine(`open:     ${result.open_files.length} file(s)`);
  } catch (err) {
    vscode.window.showErrorMessage(`Mosaic status failed: ${err}`);
  }
}

async function openDashboard(): Promise<void> {
  const server = config().get<string>("server", "");
  if (!server) {
    vscode.window.showWarningMessage(
      "Mosaic: set mosaic.server in your settings (e.g. http://localhost:7700)",
    );
    return;
  }
  await vscode.env.openExternal(vscode.Uri.parse(server));
}
