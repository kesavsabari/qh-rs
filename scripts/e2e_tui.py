#!/usr/bin/env python3
"""Drive the qh TUI in a real terminal (ConPTY on Windows, a pty elsewhere) against a tiny model,
reading the screen the way a person would. Usage: e2e_tui.py <qh binary> <model> [shots dir].
What qh itself draws and keeps hard-fails; what depends on the model's choices only warns.
Needs pyte, and pywinpty on Windows."""
import os, shutil, subprocess, sys, tempfile, threading, time
import pyte

QH, MODEL = os.path.abspath(sys.argv[1]), sys.argv[2]
SHOTS = sys.argv[3] if len(sys.argv) > 3 else None
COLS, ROWS = 140, 40
TURN = int(os.environ.get("E2E_TURN_SECS", "240"))  # how long a model turn may take
WIN = os.name == "nt"
warns = []

tmp = tempfile.mkdtemp(prefix="qh-tui-")
state, repo, home = (os.path.join(tmp, d) for d in ("state", "repo", "home"))
for d in (state, repo, home): os.makedirs(d)
cfg = os.path.join(home, "qh-config")
os.makedirs(cfg)
with open(os.path.join(state, "default_model"), "w") as f: f.write(MODEL)
# qh's own config: what b026 emptied. It must still be there, unchanged, when qh quits
KEEP = os.path.join(cfg, "config.toml")
with open(KEEP, "w") as f: f.write('# kept by the e2e\n')
with open(os.path.join(repo, "notes.txt"), "w") as f: f.write("first line\n")
for c in (["git", "init", "-q"], ["git", "add", "."], ["git", "-c", "user.name=e2e", "-c", "user.email=e@e", "commit", "-qm", "init"]):
    subprocess.run(c, cwd=repo, check=True)
env = dict(os.environ, QH_STATE_DIR=state, QH_CONFIG=KEEP, HOME=home, USERPROFILE=home, TERM="xterm-256color",
           QH_NO_UPDATE_CHECK="1")

screen = pyte.Screen(COLS, ROWS)
stream = pyte.ByteStream(screen)
lock = threading.Lock()

if WIN:
    from winpty import PtyProcess
    p = PtyProcess.spawn([QH], cwd=repo, env=env, dimensions=(ROWS, COLS))
    def read():
        while p.isalive():
            try: d = p.read(65536)
            except EOFError: break
            if d:
                with lock: stream.feed(d.encode("utf-8", "replace") if isinstance(d, str) else d)
    def write(s): p.write(s)
    def alive(): return p.isalive()
    def code(): return p.exitstatus
else:
    import pty, fcntl, struct, termios
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(repo)
        os.execve(QH, [QH], env)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    done = {}
    def read():
        while True:
            try: d = os.read(fd, 65536)
            except OSError: break
            if not d: break
            with lock: stream.feed(d)
    def write(s): os.write(fd, s.encode())
    def alive():
        if "code" in done: return False
        r, st = os.waitpid(pid, os.WNOHANG)
        if r: done["code"] = os.waitstatus_to_exitcode(st)
        return not r
    def code(): return done.get("code")

threading.Thread(target=read, daemon=True).start()

def text():
    with lock: return "\n".join(screen.display)

n = [0]
def shot(name):
    n[0] += 1
    if SHOTS:
        os.makedirs(SHOTS, exist_ok=True)
        with open(os.path.join(SHOTS, f"{n[0]:02d}-{name}.txt"), "w", encoding="utf-8") as f: f.write(text() + "\n")

def die(m):
    print("FAIL:", m); print(text()); shot("fail")
    try: write("\x11")
    except Exception: pass
    sys.exit(1)

def warn(m):
    print("WARN:", m); warns.append(m)

def wait(pred, secs, what, hard=True):
    end = time.time() + secs
    while time.time() < end:
        t = text()
        if pred(t): return t
        if not alive(): die(f"qh exited ({code()}) waiting for {what}")
        time.sleep(0.2)
    (die if hard else warn)(f"timed out waiting for {what}")
    return None

def key(s, pause=0.4):
    write(s); time.sleep(pause)

# 1. it starts and draws: the input line, the status line, this OS's own key names
t = wait(lambda t: "message, /help" in t, 30, "the input line")
print("started")
shot("start")
keys = ("Shift+Tab", "Ctrl+P", "Ctrl+K") if WIN else ("⌃P", "⌃K") if sys.platform == "darwin" else ("^P", "^K")
for k in keys:
    if k not in t: die(f"footer doesn't name {k} the way this OS writes it")
if "repo" not in t: die("the status line doesn't show the folder")

# 2. the everything palette (Ctrl+K), searched, and closed again
key("\x0b")
wait(lambda t: "search:" in t and "/model" in t, 5, "the Ctrl+K palette")
shot("palette")
key("sandbox")
wait(lambda t: "/sandbox" in t and "/model " not in t, 5, "the palette filtered to /sandbox")
key("\x1b")
wait(lambda t: "search:" not in t, 5, "the palette to close")

# 3. a command: /sandbox says what commands may touch, Windows included
key("/sandbox", 0.2); key("\r", 1.0)
t = wait(lambda t: "sandbox" in t.lower() and ("protect" in t.lower() or "read-only" in t.lower() or "write" in t.lower()), 10, "/sandbox's answer")
shot("sandbox")

# 4. modes cycle with Shift+Tab and come back round
before = [m for m in ("[ask]", "[auto-edit]", "[auto]", "[plan]") if m in text()]
key("\x1b[Z", 0.6)
after = [m for m in ("[ask]", "[auto-edit]", "[auto]", "[plan]") if m in text()]
if not before or before == after: die(f"Shift+Tab didn't change the mode: {before} -> {after}")
for _ in range(3): key("\x1b[Z", 0.4)
if [m for m in ("[ask]", "[auto-edit]", "[auto]", "[plan]") if m in text()] != before: die("four Shift+Tabs didn't come back to the first mode")

def ran(word):
    """A screen line with the command's output: not the prompt we typed, not the command itself."""
    return lambda t: any(word in l and "echo" not in l and "▌" not in l for l in t.splitlines())

# 5. a model turn that runs a command in the sandbox, and one that tries to plant a protected file
key("Use the bash tool to run exactly: echo qh-e2e-ok   Then say done.", 0.2); key("\r", 0.5)
t = wait(ran("qh-e2e-ok"), TURN, "the model to run echo", hard=False)
shot("turn")
key("Use the bash tool to run exactly: echo planted > .envrc   Then say done.", 0.2); key("\r", 0.5)
wait(lambda t: any(".envrc" in l and "▌" not in l and "Use the bash" not in l for l in t.splitlines()), TURN, "the model to try .envrc", hard=False)
time.sleep(2)
shot("envrc")
if os.path.exists(os.path.join(repo, ".envrc")):
    die("a sandboxed command left a new .envrc in the project")

# 6. quits cleanly, and qh's own files are all still there
key("\x11", 0.5)
end = time.time() + 15
while alive() and time.time() < end: time.sleep(0.2)
if alive(): die("Ctrl+Q didn't quit")
if code() not in (0, None): die(f"qh exited with {code()}")
if not os.path.exists(KEEP) or open(KEEP).read() != "# kept by the e2e\n": die("qh's config.toml changed or vanished (b026)")
print(f"OK ({len(warns)} warnings)")
shutil.rmtree(tmp, ignore_errors=True)
