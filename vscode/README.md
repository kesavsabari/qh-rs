# qh for VS Code

[qh](https://github.com/kesavsabari/qh-rs) is a coding agent for your terminal: several sessions that ask, answer and hand work to each other, on local models, cloud models or your ChatGPT plan, with your team on the same project over qh-link.

This extension puts qh beside your code.

- **Open qh**: the ✦ button at the top right of an editor, the `✦ qh` item in the status bar, or <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>Q</kbd> (<kbd>Cmd</kbd>+<kbd>Alt</kbd>+<kbd>Q</kbd> on a Mac). qh opens in a tab beside your code, in your project folder, with everything it has in the terminal: sessions, team mode, the sandbox.
- **Send your selection**: select code and press <kbd>Ctrl</kbd>+<kbd>Alt</kbd>+<kbd>L</kbd> (<kbd>Cmd</kbd>+<kbd>Alt</kbd>+<kbd>L</kbd>), or right-click → *Send selection to qh*. qh gets `@file (lines 10-20)` in its input; type your request after it.
- **Send a file**: right-click a file in the editor or the explorer → *Send file to qh*.
- **Review changes**: when qh edits files, the status bar shows `± 3 changes`. Click it, pick a file, and see the diff in VS Code's diff view.

qh itself must be installed: the extension offers to run the install line if it can't find it. Settings: `qh.path` (where qh is, if not found on its own) and `qh.openIn` (`beside` or `panel`).

Works in VS Code, Cursor, Windsurf and VSCodium, on Windows, macOS and Linux.
