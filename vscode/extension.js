// qh in VS Code, first step: qh itself runs in an editor terminal beside your code; the extension
// hands it your selection or file as an @mention and shows what it changed in VS Code's diff view.
// Later steps attach to `qh serve` instead of a terminal (see PLAN.md, v0.3.0).
const vscode = require('vscode');
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFile } = require('child_process');

let term;               // the qh terminal, while it runs
const changed = new Set(); // files changed on disk while qh runs, not by your own saves
const saved = new Map();   // file → when you last saved it in the editor
let openItem, changesItem;

const cfg = (k) => vscode.workspace.getConfiguration('qh').get(k);
const root = () => vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;

// VS Code started from the Dock doesn't get the shell's PATH, so also look where the install lines put qh
function candidates() {
  const own = cfg('path');
  if (own) return [own];
  const home = os.homedir();
  return process.platform === 'win32'
    ? ['qh.exe', path.join(process.env.LOCALAPPDATA || '', 'qh', 'bin', 'qh.exe')]
    : ['qh', path.join(home, '.local', 'bin', 'qh'), '/opt/homebrew/bin/qh', '/usr/local/bin/qh'];
}

function version(exe) {
  return new Promise((res) => execFile(exe, ['--version'], { timeout: 5000 }, (err, out) => res(err ? null : String(out).trim())));
}

async function findQh() {
  for (const exe of candidates()) {
    if (path.isAbsolute(exe) && !fs.existsSync(exe)) continue;
    if (await version(exe)) return exe;
  }
  return null;
}

async function offerInstall() {
  const pick = await vscode.window.showWarningMessage("qh isn't installed (or not where VS Code can find it).", 'Install qh', 'Set path…');
  if (pick === 'Install qh') vscode.commands.executeCommand('qh.install');
  if (pick === 'Set path…') vscode.commands.executeCommand('workbench.action.openSettings', 'qh.path');
}

async function open() {
  if (term && term.exitStatus === undefined) { term.show(); return term; }
  const exe = await findQh();
  if (!exe) { offerInstall(); return undefined; }
  term = vscode.window.createTerminal({
    name: 'qh',
    shellPath: exe,
    cwd: root(),
    iconPath: new vscode.ThemeIcon('sparkle'),
    location: cfg('openIn') === 'panel' ? vscode.TerminalLocation.Panel : { viewColumn: vscode.ViewColumn.Beside },
    env: { QH_EDITOR: 'vscode' },
  });
  term.fresh = Date.now();
  changed.clear();
  update();
  term.show();
  return term;
}

// qh reads `@path` from what you type; a just-started qh gets a moment to draw its input first
async function send(text) {
  const t = await open();
  if (!t) return;
  const wait = 1500 - (Date.now() - t.fresh);
  if (wait > 0) await new Promise((r) => setTimeout(r, wait));
  t.sendText(text, false);
  t.show();
}

function mention(uri) {
  const rel = vscode.workspace.asRelativePath(uri, false);
  // qh splits mentions on spaces: a path with one goes in as plain text instead
  return /\s/.test(rel) ? `"${rel}"` : `@${rel}`;
}

async function sendSelection() {
  const ed = vscode.window.activeTextEditor;
  if (!ed) return;
  const s = ed.selection;
  const lines = s.isEmpty ? '' : s.start.line === s.end.line ? ` (line ${s.start.line + 1})` : ` (lines ${s.start.line + 1}-${s.end.line + 1})`;
  await send(`${mention(ed.document.uri)}${lines} `);
}

async function sendFile(uri) {
  const u = uri instanceof vscode.Uri ? uri : vscode.window.activeTextEditor?.document.uri;
  if (u) await send(`${mention(u)} `);
}

async function reviewChanges() {
  if (!changed.size) { vscode.window.showInformationMessage('qh hasn\'t changed any files yet.'); return; }
  const items = [...changed].sort().map((f) => ({ label: vscode.workspace.asRelativePath(f, false), file: f }));
  const pick = await vscode.window.showQuickPick(items, { placeHolder: 'Files qh changed: pick one to see its diff' });
  if (!pick) return;
  const uri = vscode.Uri.file(pick.file);
  // the git diff when there is one, else the file itself
  try { await vscode.commands.executeCommand('git.openChange', uri); } catch { await vscode.window.showTextDocument(uri); }
}

function install() {
  const t = vscode.window.createTerminal({ name: 'install qh' });
  t.show();
  t.sendText(process.platform === 'win32' ? 'irm kesavsabari.github.io/qh | iex' : 'curl -fsSL kesavsabari.github.io/qh/sh | sh', true);
}

const ignored = (f) => /[\\/](\.git|node_modules|target|dist|build|\.next|__pycache__)[\\/]/.test(f);

function onDisk(uri) {
  if (!term || term.exitStatus !== undefined) return;
  const f = uri.fsPath;
  if (ignored(f) || (fs.existsSync(f) && fs.statSync(f).isDirectory())) return;
  if (Date.now() - (saved.get(f) || 0) < 2000) return; // your own save, not qh's edit
  changed.add(f);
  update();
}

function update() {
  const running = term && term.exitStatus === undefined;
  openItem.text = '$(sparkle) qh';
  openItem.tooltip = running ? 'Show qh' : 'Open qh beside your code';
  if (running && changed.size) {
    changesItem.text = `$(diff) ${changed.size} change${changed.size === 1 ? '' : 's'}`;
    changesItem.tooltip = "Review the files qh changed";
    changesItem.show();
  } else changesItem.hide();
}

function activate(ctx) {
  openItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
  openItem.command = 'qh.open';
  changesItem = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 99);
  changesItem.command = 'qh.reviewChanges';
  update();
  openItem.show();

  const watcher = vscode.workspace.createFileSystemWatcher('**/*');
  ctx.subscriptions.push(
    openItem, changesItem, watcher,
    vscode.commands.registerCommand('qh.open', open),
    vscode.commands.registerCommand('qh.sendSelection', sendSelection),
    vscode.commands.registerCommand('qh.sendFile', sendFile),
    vscode.commands.registerCommand('qh.reviewChanges', reviewChanges),
    vscode.commands.registerCommand('qh.install', install),
    watcher.onDidChange(onDisk),
    watcher.onDidCreate(onDisk),
    vscode.workspace.onWillSaveTextDocument((e) => saved.set(e.document.uri.fsPath, Date.now())),
    vscode.workspace.onDidSaveTextDocument((d) => saved.set(d.uri.fsPath, Date.now())),
    vscode.window.onDidCloseTerminal((t) => { if (t === term) { term = undefined; changed.clear(); update(); } }),
  );
  // for tests and other extensions: what qh changed while it ran
  return { changedFiles: () => [...changed] };
}

function deactivate() {}

module.exports = { activate, deactivate };
