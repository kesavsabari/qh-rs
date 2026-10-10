"""Fixed tasks to check whether prompt/tool changes help. Each runs the qh
binary in a fresh temp dir (its own state dir, so no real history is touched)
with every confirmation auto-approved. Same tasks as the Python harness.

    python evals/run.py [--runs 3] [--save] [--only a,b] [--model provider:model, e.g. openrouter:qwen/qwen3-coder or gpt-oss:20b-cloud] [--ctx 8192]
"""
import argparse
import json
import os
import re
import subprocess
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
QH = os.environ.get("QH_BIN", str(ROOT.parent / "target/release/qh"))
RESULTS = ROOT / "results"
FAILS = ROOT / "fails"
PY_RESULTS = Path.home() / "Work/qwen-harness/evals/results"
USER_CONFIG = Path(os.environ.get("QH_CONFIG", Path.home() / ".config/qwen-harness/config.toml"))


def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True).stdout.strip()


def ok_run(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True).returncode == 0


def text(p):
    return Path(p).read_text() if Path(p).exists() else ""


# (name, files, prompt, check, turns) — extra turns are sent after the prompt
TASKS = [
    ("fix_bug", {"m.py": "def add(a, b):\n    return a - b\n"},
     "add() in m.py is wrong. Fix it.",
     lambda: sh("python -c 'import m;print(m.add(2,3))'") == "5"),

    ("new_file", {},
     "Create fizz.py with fizzbuzz(n) returning 'Fizz', 'Buzz', 'FizzBuzz' or str(n).",
     lambda: sh("python -c 'from fizz import fizzbuzz as f;print(f(3),f(5),f(15),f(7))'") == "Fizz Buzz FizzBuzz 7"),

    ("find_usage", {"a.py": "from b import helper\nhelper()\n", "b.py": "def helper():\n    pass\n",
                    "c.py": "x = 1\n"},
     "Which files call helper()? Write their names, one per line, to answer.txt.",
     lambda: "a.py" in text("answer.txt") and "c.py" not in text("answer.txt")),

    ("rename", {"calc.py": "def mul(x, y):\n    return x * y\n", "main.py": "from calc import mul\nprint(mul(3, 4))\n"},
     "Rename mul to multiply everywhere.",
     lambda: sh("python main.py") == "12" and "mul(" not in text("calc.py").replace("multiply(", "")),

    ("add_test", {"s.py": "def slug(t):\n    return t.lower().replace(' ', '-')\n"},
     "Write test_s.py with at least two assert-based tests for slug, then run it with python.",
     lambda: ok_run("python -c 'import test_s,inspect;[f() for n,f in inspect.getmembers(test_s,inspect.isfunction)"
                    " if n.startswith(\"test\")]'") and text("test_s.py").count("assert") >= 2),

    ("multi_file", {"models.py": "class User:\n    def __init__(self, name):\n        self.name = name\n",
                    "app.py": "from models import User\nu = User('ann')\nprint(u.name)\n"},
     "Add an email field to User (constructor arg, default None) and make app.py print the name and email "
     "for User('ann', 'a@x.io'), space separated.",
     lambda: sh("python app.py") == "ann a@x.io"),

    ("import_move", {"utils.py": "def clean(s):\n    return s.strip()\n",
                     "main.py": "from utils import clean\nprint(clean('  hi  '))\n"},
     "Move utils.py into a package at lib/text.py (create lib/__init__.py) and update main.py so it still works. "
     "Delete the old utils.py.",
     lambda: sh("python main.py") == "hi" and not Path("utils.py").exists() and Path("lib/text.py").exists()),

    ("stack_trace", {"stats.py": "def mean(xs):\n    return sum(xs) / len(xs)\n",
                     "run.py": "from stats import mean\nprint(mean([]))\n"},
     "Running `python run.py` crashes. Make mean() return 0 for an empty list instead.",
     lambda: sh("python run.py") in ("0", "0.0") and sh("python -c 'import stats;print(stats.mean([2,4]))'") == "3.0"),

    ("pytest_fix", {"shop.py": "def total(prices, tax):\n    return sum(prices) + tax\n",
                    "test_shop.py": "from shop import total\n\ndef test_total():\n"
                                    "    assert total([10, 20], 0.1) == 33.0\n"},
     "The test in test_shop.py fails. tax is a rate (0.1 = 10%). Fix shop.py, not the test.",
     lambda: ok_run("python -c 'import test_shop;test_shop.test_total()'")
     and "33.0" in text("test_shop.py")),

    ("dont_touch", {"core.py": "LIMIT = 10\n\ndef ok(n):\n    return n < LIMIT\n",
                    "config.py": "DEBUG = False\n"},
     "Make ok() also accept n == LIMIT. Do not modify config.py.",
     lambda: sh("python -c 'import core;print(core.ok(10), core.ok(11))'") == "True False"
     and text("config.py") == "DEBUG = False\n"),

    ("json_edit", {"settings.json": '{\n  "port": 8000,\n  "debug": true,\n  "name": "app"\n}\n'},
     "In settings.json set port to 9000 and debug to false. Keep the other keys.",
     lambda: json.loads(text("settings.json") or "{}") == {"port": 9000, "debug": False, "name": "app"}),

    ("count_lines", {"a.txt": "x\n" * 7, "b.txt": "y\n" * 12, "c.md": "z\n" * 3},
     "How many lines are in all .txt files combined? Write just the number to answer.txt.",
     lambda: text("answer.txt").strip() == "19"),

    ("refactor_dup", {"report.py": "def a(xs):\n    return round(sum(xs) / len(xs), 2)\n\n"
                                   "def b(ys):\n    return round(sum(ys) / len(ys), 2)\n"},
     "a and b in report.py duplicate logic. Extract a helper avg(v) and make both call it.",
     lambda: "def avg" in text("report.py") and text("report.py").count("sum(") == 1
     and sh("python -c 'import report;print(report.a([1,2]),report.b([3,4]))'") == "1.5 3.5"),

    ("cli_args", {"greet.py": "print('hello')\n"},
     "Change greet.py so `python greet.py NAME` prints `hello NAME`, and plain `python greet.py` still prints `hello`.",
     lambda: sh("python greet.py bob") == "hello bob" and sh("python greet.py") == "hello"),

    ("long_session", {"big.txt": "".join(f"row {i} value {i * 7}\n" for i in range(1, 1201))},
     "Read big.txt lines 1-400 and tell me the value on row 50.",
     lambda: all(v in text("answer.txt") for v in ("350", "4900", "7700")),
     ["Now read lines 400-800 and tell me the value on row 700.",
      "Now read lines 800-1200 and tell me the value on row 1100.",
      "Write the three values you found, one per line, to answer.txt."]),
]


SITE = "http://127.0.0.1:8799"
# (name, prompt, check) — run with the browser profile against evals/sites
BROWSER_TASKS = [
    ("web_find", f"What is the price of the Gadget at {SITE}/ ? Write just the number (no currency) to answer.txt.",
     lambda: text("answer.txt").strip().lstrip("€").startswith("48")),
    ("web_chain", f"Starting at {SITE}/ , find how many grams the Gadget weighs. Write just the number to answer.txt.",
     lambda: text("answer.txt").strip() == "340"),
    ("web_form", f"Use the contact form at {SITE}/ to send a Billing message from the name Ann saying 'refund please'. "
                 "Then write the URL of the page you end up on to answer.txt.",
     lambda: all(x in text("answer.txt") for x in ("name=Ann", "topic=Billing", "refund"))),
]


def serve_sites():
    import http.server
    import threading
    from functools import partial
    class Quiet(http.server.SimpleHTTPRequestHandler):
        def log_message(self, *a):
            pass
    handler = partial(Quiet, directory=str(ROOT / "sites"))
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 8799), handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


def eval_config(d):
    """The user's config with the browser forced headless and allowed onto the local test site."""
    body = USER_CONFIG.read_text() if USER_CONFIG.exists() else ""
    body = "\n".join(l for l in body.splitlines() if not re.match(r"\s*browser_(headless|allow_hosts)\s*=", l))
    # top-level keys must come before any [[table]], so prepend
    p = Path(d) / ".qh-eval.toml"
    p.write_text('browser_headless = true\nbrowser_allow_hosts = ["127.0.0.1"]\n' + body + "\n")
    return p


def run_task(task, model, ctx):
    name, files, prompt, check, *more = task
    profile = "browser" if name.startswith("web_") else "coding"
    turns = [prompt] + (more[0] if more else [])
    home = os.getcwd()
    with tempfile.TemporaryDirectory() as d, tempfile.TemporaryDirectory() as state:
        os.chdir(d)
        for f, body in files.items():
            Path(f).write_text(body)
        cfg = eval_config(state)
        stats = Path(state) / "stats.json"
        cmd = [QH, "-y", "--profile", profile, "--stats", str(stats)]
        if model:
            cmd += ["--model", model]
        if ctx:
            cmd += ["--ctx", str(ctx)]
        for t in turns:
            cmd += ["-p", t]
        env = dict(os.environ, QH_CONFIG=str(cfg), QH_STATE_DIR=state)
        t0, err, r = time.time(), "", None
        try:
            r = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=900)
            if r.returncode:
                err = (r.stderr.strip().splitlines() or ["exit %d" % r.returncode])[-1]
            ok = bool(check())
        except Exception as e:
            ok, err, r = False, f"{type(e).__name__}: {e}", None
        s = json.loads(stats.read_text()) if stats.exists() else {}
        if not ok:  # keep the transcript and the final files for a look later
            out = FAILS / f"{name}-{time.strftime('%H%M%S')}"
            out.mkdir(parents=True, exist_ok=True)
            sess = next(Path(state, "sessions").glob("*.jsonl"), None)
            if sess:
                (out / "session.jsonl").write_text(sess.read_text())
            if r is not None:
                (out / "stdout.txt").write_text(r.stdout)
                (out / "stderr.txt").write_text(r.stderr)
            (out / "files.txt").write_text("\n".join(
                f"--- {f}\n{f.read_text(errors='replace')[:2000]}" for f in Path(d).rglob("*") if f.is_file()))
        os.chdir(home)
    return {"ok": ok, "tokens": s.get("tokens", 0), "calls": s.get("calls", 0), "compacts": s.get("compacts", 0),
            "secs": round(time.time() - t0, 1), "err": err}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--model")
    ap.add_argument("--only", help="comma-separated task names")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--ctx", type=int, help="override context window (e.g. 8192 to stress compaction)")
    ap.add_argument("--save", action="store_true")
    a = ap.parse_args()
    only = set(a.only.split(",")) if a.only else None
    all_tasks = TASKS + [(n, {}, p, c) for n, p, c in BROWSER_TASKS]
    tasks = [t for t in all_tasks if not only or t[0] in only]
    if any(t[0].startswith("web_") for t in tasks):
        serve_sites()

    summary = {}
    for task in tasks:
        runs = [run_task(task, a.model, a.ctx) for _ in range(a.runs)]
        n = len(runs)
        s = {"pass": sum(r["ok"] for r in runs) / n,
             "tokens": round(sum(r["tokens"] for r in runs) / n),
             "calls": round(sum(r["calls"] for r in runs) / n, 1),
             "secs": round(sum(r["secs"] for r in runs) / n, 1)}
        summary[task[0]] = s
        errs = "  " + "; ".join({r["err"] for r in runs if r["err"]}) if any(r["err"] for r in runs) else ""
        marks = "".join("✓" if r["ok"] else "✗" for r in runs)
        print(f"{marks:<4} {task[0]:<13} {s['pass']:>4.0%}  {s['tokens']:>6} tok  {s['calls']:>4} calls  "
              f"{s['secs']:>5}s{errs[:120]}", flush=True)

    total = sum(s["pass"] for s in summary.values()) / len(summary)
    print(f"\npass rate {total:.0%} over {len(summary)} tasks × {a.runs} runs")

    for label, folder in (("rust", RESULTS), ("python", PY_RESULTS)):
        files = sorted(folder.glob("*.json")) if folder.exists() else []
        if not files:
            continue
        prev = files[-1]
        old = json.loads(prev.read_text())["tasks"]
        diffs = [f"  {k}: {old[k]['pass']:.0%} → {v['pass']:.0%}" for k, v in summary.items()
                 if k in old and old[k]["pass"] != v["pass"]]
        print(f"vs {label} {prev.stem} ({json.loads(prev.read_text())['pass']:.0%}):" + ("\n" + "\n".join(diffs) if diffs else " no pass-rate changes"))
    if a.save:
        RESULTS.mkdir(exist_ok=True)
        out = RESULTS / f"{time.strftime('%Y%m%d-%H%M%S')}.json"
        out.write_text(json.dumps({"model": a.model or "local", "runs": a.runs, "pass": total,
                                   "tasks": summary}, indent=1))
        print(f"saved {out}")


if __name__ == "__main__":
    main()
