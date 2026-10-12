//! Background commands: `bash` with background=true. Each task belongs to a tab, keeps only its
//! last LINES lines of output, and is killed (its whole process group) when the tab closes or
//! qh exits.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::process::Child;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

pub const LINES: usize = 2000;
pub const MAX_PER_TAB: usize = 8;
const LINE_CHARS: usize = 2000;

/// The last LINES lines of a task's output (stdout and stderr interleaved).
#[derive(Default)]
pub struct Ring {
    lines: VecDeque<String>,
    /// lines ever pushed: line n (0-based) is in the ring when n >= total - lines.len()
    total: u64,
    cap: usize,
}

impl Ring {
    pub fn new(cap: usize) -> Ring {
        Ring { lines: VecDeque::new(), total: 0, cap }
    }

    pub fn push(&mut self, mut l: String) {
        if l.len() > LINE_CHARS {
            let mut i = LINE_CHARS;
            while !l.is_char_boundary(i) { i -= 1 }
            l.truncate(i);
            l.push('…');
        }
        if self.lines.len() == self.cap {
            self.lines.pop_front();
        }
        self.lines.push_back(l);
        self.total += 1;
    }

    /// Lines after `seen` (a line count), and how many of them were already dropped.
    pub fn since(&self, seen: u64) -> (Vec<String>, u64) {
        let first = self.total - self.lines.len() as u64;
        let dropped = first.saturating_sub(seen);
        let skip = seen.saturating_sub(first) as usize;
        (self.lines.iter().skip(skip).cloned().collect(), dropped)
    }
}

struct Task {
    id: String,
    cmd: String,
    start: Instant,
    end: Option<Instant>,
    child: Child,
    out: Arc<Mutex<Ring>>,
    readers: Vec<std::thread::JoinHandle<()>>,
    seen: u64,
    exit: Option<String>,
    snap: Option<crate::sandbox::Snapshot>,
}

impl Task {
    /// Notice an exit; put back protected files a sandboxed run changed.
    fn poll(&mut self) {
        if self.exit.is_some() {
            return;
        }
        if let Ok(Some(s)) = self.child.try_wait() {
            self.finish(s.code().map(|c| format!("exited {c}")).unwrap_or_else(|| "killed".into()));
        }
    }

    fn finish(&mut self, how: String) {
        // let the readers flush what the pipe still holds (a grandchild may keep it open: don't wait long)
        let t0 = Instant::now();
        while self.readers.iter().any(|r| !r.is_finished()) && t0.elapsed() < Duration::from_millis(300) {
            std::thread::sleep(Duration::from_millis(10));
        }
        self.exit = Some(how);
        self.end = Some(Instant::now());
        if let Some(s) = self.snap.take() {
            let fixed = s.restore();
            if !fixed.is_empty() {
                let names: Vec<String> = fixed.iter().map(|f| f.display().to_string()).collect();
                lock(&self.out).push(format!("qh sandbox: put back {} (protected: it runs code later, outside the sandbox)", names.join(", ")));
            }
        }
    }

    fn kill(&mut self) {
        if self.exit.is_some() {
            return;
        }
        kill_tree(self.child.id());
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.finish("stopped".into());
    }

    fn status(&self) -> String {
        self.exit.clone().unwrap_or_else(|| "running".into())
    }

    fn runtime(&self) -> String {
        let s = self.end.unwrap_or_else(Instant::now).duration_since(self.start).as_secs();
        if s >= 3600 { format!("{}h{:02}m", s / 3600, s / 60 % 60) } else if s >= 60 { format!("{}m{:02}s", s / 60, s % 60) } else { format!("{s}s") }
    }
}

fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill").args(["/T", "/F", "/PID", &pid.to_string()]).output();
}

impl Drop for Task {
    fn drop(&mut self) {
        self.kill();
    }
}

static TASKS: LazyLock<Mutex<HashMap<String, Vec<Task>>>> = LazyLock::new(Default::default);
static NEXT: AtomicU64 = AtomicU64::new(1);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn reader(r: Option<impl Read + Send + 'static>, out: Arc<Mutex<Ring>>) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Some(mut r) = r else { return };
        let (mut buf, mut part) = ([0u8; 8192], Vec::<u8>::new());
        while let Ok(n) = r.read(&mut buf) {
            if n == 0 { break }
            part.extend_from_slice(&buf[..n]);
            while let Some(i) = part.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = part.drain(..=i).collect();
                let l = String::from_utf8_lossy(&line[..line.len() - 1]).trim_end_matches('\r').to_string();
                lock(&out).push(l);
            }
            if part.len() > LINE_CHARS * 4 {
                lock(&out).push(String::from_utf8_lossy(&std::mem::take(&mut part)).into_owned());
            }
        }
        if !part.is_empty() {
            lock(&out).push(String::from_utf8_lossy(&part).into_owned());
        }
    })
}

/// Tasks a tab has running.
pub fn running(tab: &str) -> usize {
    let mut all = lock(&TASKS);
    all.get_mut(tab).map(|v| v.iter_mut().map(|t| { t.poll(); t.exit.is_none() }).filter(|r| *r).count()).unwrap_or(0)
}

/// Check the tab's room for another task before spawning.
pub fn room(tab: &str) -> Result<(), String> {
    if running(tab) >= MAX_PER_TAB {
        return Err(format!("{MAX_PER_TAB} background tasks already run in this tab: stop one with task_stop first"));
    }
    Ok(())
}

/// Register a spawned child, wait up to `wait` for its first output, and say what it printed.
pub fn start(tab: &str, cmd: &str, mut child: Child, snap: Option<crate::sandbox::Snapshot>, wait: Duration) -> String {
    let out = Arc::new(Mutex::new(Ring::new(LINES)));
    let readers = vec![reader(child.stdout.take(), out.clone()), reader(child.stderr.take(), out.clone())];
    let id = format!("bg{}", NEXT.fetch_add(1, Relaxed));
    let mut t = Task { id: id.clone(), cmd: cmd.into(), start: Instant::now(), end: None, child, out, readers, seen: 0, exit: None, snap };
    let t0 = Instant::now();
    while t0.elapsed() < wait {
        t.poll();
        if t.exit.is_some() { break }
        std::thread::sleep(Duration::from_millis(20));
    }
    let first = take(&mut t, None);
    lock(&TASKS).entry(tab.to_string()).or_default().push(t);
    format!("started {id} in the background; task_output(\"{id}\") reads new output, task_stop(\"{id}\") stops it\n{first}")
}

/// New output since the last read, then the status.
fn take(t: &mut Task, tail: Option<usize>) -> String {
    t.poll();
    let (mut lines, mut dropped) = lock(&t.out).since(t.seen);
    t.seen += dropped + lines.len() as u64;
    if let Some(n) = tail.filter(|n| *n < lines.len()) {
        dropped += (lines.len() - n) as u64;
        lines.drain(..lines.len() - n);
    }
    let mut s = String::new();
    if dropped > 0 {
        s += &format!("({dropped} earlier lines not shown)\n");
    }
    if lines.is_empty() {
        s += "(no new output)\n";
    } else {
        s += &lines.join("\n");
        s.push('\n');
    }
    s + &format!("[{} · {} · {}]", t.id, t.status(), t.runtime())
}

fn with<R>(tab: &str, id: &str, f: impl FnOnce(&mut Task) -> R) -> Result<R, String> {
    let mut all = lock(&TASKS);
    all.get_mut(tab).and_then(|v| v.iter_mut().find(|t| t.id == id)).map(f)
        .ok_or_else(|| format!("no background task {id} in this tab"))
}

pub fn output(tab: &str, id: &str, tail: Option<usize>) -> Result<String, String> {
    with(tab, id, |t| take(t, tail))
}

pub fn stop(tab: &str, id: &str) -> Result<String, String> {
    with(tab, id, |t| {
        let was = t.exit.is_none();
        t.kill();
        if was { format!("{} stopped after {}", t.id, t.runtime()) } else { format!("{} had already {}", t.id, t.status()) }
    })
}

/// For /tasks: id, command, runtime, status.
pub fn list(tab: &str) -> String {
    let mut all = lock(&TASKS);
    let Some(v) = all.get_mut(tab).filter(|v| !v.is_empty()) else { return "no background tasks in this tab".into() };
    v.iter_mut().map(|t| {
        t.poll();
        let cmd: String = t.cmd.chars().take(60).collect();
        format!("{}  {}  {}  {}", t.id, t.runtime(), t.status(), cmd)
    }).collect::<Vec<_>>().join("\n")
}

/// The tab closed: kill its tasks.
pub fn kill_tab(tab: &str) {
    let v = lock(&TASKS).remove(tab);
    drop(v); // Drop kills each
}

/// qh is exiting: kill every task.
pub fn kill_all() {
    let all = std::mem::take(&mut *lock(&TASKS));
    drop(all);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn sh(cmd: &str) -> Child {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", cmd]);
        use std::os::unix::process::CommandExt;
        c.process_group(0);
        c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap()
    }

    #[test]
    fn ring_keeps_the_last_lines_and_counts_the_dropped() {
        let mut r = Ring::new(3);
        for i in 0..5 { r.push(i.to_string()) }
        let (l, d) = r.since(0);
        assert_eq!((l, d), (vec!["2".to_string(), "3".into(), "4".into()], 2));
        let (l, d) = r.since(4);
        assert_eq!((l, d), (vec!["4".to_string()], 0));
        let mut r = Ring::new(LINES);
        for i in 0..LINES * 3 { r.push(i.to_string()) }
        assert_eq!(r.lines.len(), LINES);
    }

    #[cfg(unix)]
    #[test]
    fn incremental_output_stop_and_cap() {
        let tab = "test-bg-tab";
        let first = start(tab, "loop", sh("i=0; while true; do echo line$i; i=$((i+1)); sleep 0.05; done"), None, Duration::from_millis(300));
        let id = first.split_whitespace().nth(1).unwrap().to_string();
        assert!(first.contains("line0") && first.contains("running"), "{first}");
        std::thread::sleep(Duration::from_millis(300));
        let more = output(tab, &id, None).unwrap();
        assert!(!more.contains("line0\n") && more.contains("running"), "{more}");
        let tail = output(tab, &id, Some(1));
        assert!(tail.is_ok());
        assert_eq!(running(tab), 1);
        let pid = with(tab, &id, |t| t.child.id()).unwrap();
        assert!(stop(tab, &id).unwrap().contains("stopped"));
        assert_eq!(running(tab), 0);
        // the whole group is gone (a killed child may linger as a zombie for a moment until init reaps it)
        let t0 = Instant::now();
        while unsafe { libc::kill(-(pid as i32), 0) } == 0 && t0.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(unsafe { libc::kill(-(pid as i32), 0) } != 0);
        assert!(list(tab).contains("stopped"));

        // a quick command: its exit is reported; a flood keeps only LINES lines
        let r = start(tab, "flood", sh("seq 1 10000; exit 3"), None, Duration::from_secs(2));
        assert!(r.contains("exited 3"), "{}", &r[r.len().saturating_sub(200)..]);
        assert!(r.contains("earlier lines not shown") && r.contains("\n10000\n"), "{}", &r[..200]);
        assert_eq!(r.lines().count(), LINES + 3);

        // too many
        for _ in 0..MAX_PER_TAB { start(tab, "sleep", sh("sleep 30"), None, Duration::ZERO); }
        assert!(room(tab).is_err());
        kill_tab(tab);
        assert_eq!(running(tab), 0);
        assert!(output(tab, &id, None).is_err());
    }
}
