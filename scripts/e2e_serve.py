#!/usr/bin/env python3
"""Drive `qh serve` against a real (tiny) model. Usage: e2e_serve.py <qh binary> <model>.
Protocol plumbing hard-fails; things that depend on the model's choices only warn."""
import json, os, socket, subprocess, sys, tempfile, time

QH, MODEL = os.path.abspath(sys.argv[1]), sys.argv[2]
seen, warns = [], []

def die(m):
    print("FAIL:", m); print("event types seen:", sorted(set(seen))); sys.exit(1)

def warn(m):
    print("WARN:", m); warns.append(m)

tmp = tempfile.mkdtemp(prefix="qh-e2e-")
state, repo = os.path.join(tmp, "state"), os.path.join(tmp, "repo")
os.makedirs(state); os.makedirs(repo)
with open(os.path.join(state, "default_model"), "w") as f: f.write(MODEL)
env = dict(os.environ, QH_STATE_DIR=state, HOME=os.path.join(tmp, "home"))
os.makedirs(env["HOME"])
NOTES = os.path.join(repo, "notes.txt")
with open(NOTES, "w") as f: f.write("first line\n")
for c in (["git", "init", "-q"], ["git", "add", "."], ["git", "-c", "user.name=e2e", "-c", "user.email=e@e", "commit", "-qm", "init"]):
    subprocess.run(c, cwd=repo, check=True)

srv = subprocess.Popen([QH, "serve"], cwd=repo, env=env, stdout=subprocess.PIPE, stderr=sys.stderr, text=True)
line = srv.stdout.readline()
print("serve:", line.strip())
try: hi = json.loads(line)
except Exception: die(f"no listening line: {line!r}")
if hi.get("type") != "listening": die(f"expected listening, got {hi}")
token = open(hi["token_file"]).read().strip()
s = socket.socket(socket.AF_UNIX); s.connect(hi["socket"]); s.settimeout(1.0)
buf = b""

def send(v): s.sendall((json.dumps(v) + "\n").encode())

def recv(deadline):
    global buf
    while time.time() < deadline:
        if b"\n" in buf:
            l, buf = buf.split(b"\n", 1)
            if not l.strip(): continue
            v = json.loads(l); seen.append(v.get("type"))
            t = v.get("type")
            short = {k: v[k] for k in v if k not in ("tabs", "output", "files", "items")} if t != "text" else None
            if t != "text": print("<-", json.dumps(short)[:300])
            return v
        try:
            d = s.recv(65536)
            if not d: die("server closed the socket")
            buf += d
        except socket.timeout:
            if srv.poll() is not None: die(f"qh serve exited {srv.returncode}")
    return None

def wait_for(t, secs=30):
    end = time.time() + secs
    while (v := recv(end)) is not None:
        if v.get("type") == t: return v
    return None

send({"type": "hello", "token": token})
snap = wait_for("snapshot")
if not snap: die("no snapshot after hello")
print("snapshot: mode", snap.get("mode"), "needs_setup", snap.get("needs_setup"), "tabs", [t.get("name") for t in snap.get("tabs", [])])
if snap.get("needs_setup"): warn("snapshot says needs_setup with a model configured")
send({"type": "mode", "mode": "ask"})

PROMPTS = [
    "Add a line 'hello' to notes.txt",
    "Use the edit_file or write_file tool to append the line hello to the file notes.txt. Do not just describe it, call the tool.",
    "Call a file-writing tool now: write notes.txt with content 'first line\\nhello\\n'.",
]
ok = False
for attempt, p in enumerate(PROMPTS, 1):
    print(f"--- attempt {attempt}: {p}")
    send({"type": "prompt", "tab": "main", "text": p})
    got = {"text": 0, "tool_call": 0, "tool_result": 0, "diff": 0, "confirm": 0}
    end, done = time.time() + 300, False
    while (v := recv(end)) is not None:
        t = v.get("type")
        if t in got: got[t] += 1
        if t == "error": print("server error:", v)
        if t == "confirm":
            send({"type": "answer", "id": v["id"], "answer": "yes", "reply": "yes", "ok": True})
        if t == "turn_done": done = True; break
    if not done: die(f"no turn_done within 300s on attempt {attempt}")
    print("counts:", got)
    if got["text"] == 0 and got["tool_call"] == 0: warn("no text and no tool call streamed")
    if got["tool_call"] and got["tool_result"] and "hello" in open(NOTES).read():
        ok = True
        if not got["confirm"]: warn("an edit ran in ask mode without a confirm")
        if not got["diff"]: warn("file edited but no diff event")
        break
    warn(f"attempt {attempt}: model didn't edit notes.txt")

if ok:
    send({"type": "changes", "tab": "main"})
    ch = wait_for("changes")
    if not ch: die("no changes reply")
    print("changes:", ch.get("files"))
    if not any("notes.txt" in json.dumps(f) for f in ch.get("files", [])): warn("changes doesn't list notes.txt")
    send({"type": "checkpoints", "tab": "main"})
    cp = wait_for("checkpoints")
    if not cp: die("no checkpoints reply")
    print("checkpoints:", json.dumps(cp.get("items"))[:500])
    if not cp.get("items"): warn("checkpoints empty after an edit")
    send({"type": "rewind", "tab": "main", "turn": 0})
    rw = wait_for("rewound")
    if not rw: die("no rewound reply")
    print("rewound:", rw)
    after = open(NOTES).read()
    if after != "first line\n": warn(f"rewind 0 didn't restore notes.txt: {after!r}")
    else: print("rewind restored notes.txt")
else:
    warn("model never called a tool in 3 attempts: tool/diff/changes/rewind unverified")

print("event types seen:", sorted(set(seen)))
print("warnings:", len(warns)); [print("  -", w) for w in warns]
srv.terminate()
print("PASS (protocol)")
