//! macOS sandbox: runs a command under Seatbelt (`sandbox-exec`) with a deny-default profile.
//! `(allow default)` would leave launchd, AppleEvents and LaunchServices open, so `osascript` or
//! `open` could ask an unsandboxed process to run anything; deny-default closes those.
//! Paths reach the profile as `-D NAME=path` parameters, never as SBPL text, so no quoting.
//! Seatbelt is last-match-wins: every deny below comes after the allow it narrows.
use crate::sandbox::{Net, Policy};
use std::path::PathBuf;

// Base rules adapted from OpenAI Codex's seatbelt_base_policy.sbpl, Apache-2.0
// (github.com/openai/codex, codex-rs/sandboxing/src); tty and Unix-socket handling informed by
// Anthropic's sandbox-runtime, Apache-2.0.
const BASE: &str = r##"(version 1)
(deny default)
(allow process-exec)
(allow process-fork)
(allow signal (target same-sandbox))
(allow process-info* (target same-sandbox))
(allow sysctl-read
  (sysctl-name "hw.activecpu") (sysctl-name "hw.busfrequency_compat") (sysctl-name "hw.byteorder")
  (sysctl-name "hw.cacheconfig") (sysctl-name "hw.cachelinesize_compat") (sysctl-name "hw.cpufamily")
  (sysctl-name "hw.cpufrequency") (sysctl-name "hw.cpufrequency_compat") (sysctl-name "hw.cputype")
  (sysctl-name "hw.l1dcachesize_compat") (sysctl-name "hw.l1icachesize_compat")
  (sysctl-name "hw.l2cachesize_compat") (sysctl-name "hw.l3cachesize_compat")
  (sysctl-name "hw.logicalcpu") (sysctl-name "hw.logicalcpu_max") (sysctl-name "hw.machine")
  (sysctl-name "hw.model") (sysctl-name "hw.memsize") (sysctl-name "hw.ncpu") (sysctl-name "hw.nperflevels")
  (sysctl-name "hw.packages") (sysctl-name "hw.pagesize") (sysctl-name "hw.pagesize_compat")
  (sysctl-name "hw.physicalcpu") (sysctl-name "hw.physicalcpu_max") (sysctl-name "hw.tbfrequency_compat")
  (sysctl-name "hw.vectorunit") (sysctl-name "machdep.cpu.brand_string") (sysctl-name "kern.argmax")
  (sysctl-name "kern.hostname") (sysctl-name "kern.maxfilesperproc") (sysctl-name "kern.maxproc")
  (sysctl-name "kern.osproductversion") (sysctl-name "kern.osrelease") (sysctl-name "kern.ostype")
  (sysctl-name "kern.osvariant_status") (sysctl-name "kern.osversion") (sysctl-name "kern.secure_kernel")
  (sysctl-name "kern.sysv.semmns") (sysctl-name "kern.usrstack64") (sysctl-name "kern.version")
  (sysctl-name "sysctl.proc_cputype") (sysctl-name "vm.loadavg")
  (sysctl-name-prefix "hw.optional.arm.") (sysctl-name-prefix "hw.optional.armv8_")
  (sysctl-name-prefix "hw.perflevel") (sysctl-name-prefix "kern.proc.pgrp.")
  (sysctl-name-prefix "kern.proc.pid.") (sysctl-name-prefix "net.routetable."))
(allow sysctl-write (sysctl-name "kern.grade_cputype"))
(allow iokit-open (iokit-registry-entry-class "RootDomainUserClient"))
(allow mach-lookup
  (global-name "com.apple.system.opendirectoryd.libinfo")
  (global-name "com.apple.system.opendirectoryd.membership")
  (global-name "com.apple.bsd.dirhelper")
  (global-name "com.apple.PowerManagement.control")
  (global-name "com.apple.cfprefsd.daemon")
  (global-name "com.apple.cfprefsd.agent")
  (local-name "com.apple.cfprefsd.agent"))
(allow user-preference-read)
(allow ipc-posix-shm-read* (ipc-posix-name-prefix "apple.cfprefs."))
(allow ipc-posix-sem)
(allow ipc-posix-shm-read-data ipc-posix-shm-write-create ipc-posix-shm-write-unlink
  (ipc-posix-name-regex #"^/__KMP_REGISTERED_LIB_[0-9]+$"))
; unconnected AF_UNIX sockets (socketpair IPC); connecting to a socket path is network-outbound
(allow system-socket (socket-domain AF_UNIX))
(allow file-read* file-test-existence)
; but not the user's terminal: reading it steals keystrokes, ioctl TIOCSTI injects them
(deny file-read* file-write* file-ioctl (literal "/dev/tty") (regex #"^/dev/ttys[0-9]+$"))
; ptys the command opens itself carry the pty extension
(allow pseudo-tty)
(allow file-read* file-write* file-ioctl (literal "/dev/ptmx"))
(allow file-read* file-write* file-ioctl
  (require-all (regex #"^/dev/ttys[0-9]+$") (extension "com.apple.sandbox.pty")))
(allow file-write-data file-ioctl (literal "/dev/null") (literal "/dev/zero") (literal "/dev/dtracehelper"))
; /dev/fd/N dups an fd the command already holds: nothing new
(allow file-write-data (literal "/dev/stdout") (literal "/dev/stderr") (regex #"^/dev/fd/[0-9]+$"))
"##;

// A command may listen (bind/inbound use *:* because a dual-stack socket sees 127.0.0.1 as
// ::ffff:127.0.0.1, which "localhost" misses), but connects only to the proxy's port: other local
// services are ways out (qh's own Chrome debug port reads any file; Ollama's API reads paths it is
// given). macOS has no network namespace to give a command a loopback of its own, so tests that talk
// to their own local server need an approved unsandboxed run here.
const LOOPBACK: &str = r##"(allow network-bind (local ip "*:*"))
(allow network-inbound (local ip "*:*"))
"##;

// Unrestricted IP, but no Unix-socket daemons (docker, tmux, ssh-agent): those are ways out of the
// sandbox, not network. DNS goes through mDNSResponder's socket.
const OPEN: &str = r##"(allow network*)
(deny network-outbound (remote unix-socket (path-regex #"^/")))
(allow network-outbound (remote unix-socket (subpath "/private/var/run/mDNSResponder"))
  (remote unix-socket (subpath "/var/run/mDNSResponder")))
"##;

// TLS and network config services, from Codex's seatbelt_network_policy.sbpl. Proxy mode gets no DNS:
// clients send CONNECT host to the proxy, and DNS would be a side channel past the allowlist.
const NET_SERVICES: &str = r##"(allow system-socket (require-all (socket-domain AF_SYSTEM) (socket-protocol 2)))
(allow mach-lookup
  (global-name "com.apple.SecurityServer") (global-name "com.apple.networkd") (global-name "com.apple.ocspd")
  (global-name "com.apple.trustd.agent") (global-name "com.apple.TrustEvaluationAgent")
  (global-name "com.apple.SystemConfiguration.DNSConfiguration")
  (global-name "com.apple.SystemConfiguration.configd"))
(allow sysctl-read (sysctl-name-regex #"^net.routetable"))
"##;

// XPC services bundled in apps, and fcntls that write through read-only fds (F_MAKECOMPRESSED,
// F_TRANSFEREXTENTS), which deny-default alone doesn't stop (per Codex).
const TAIL: &str = r##"(deny mach-lookup (xpc-service-name-prefix ""))
(deny system-fcntl (fcntl-command 80 110))
"##;

/// Directories whose rename would carry a protected or secret path out from under its rule:
/// every ancestor inside a write root, plus the write roots themselves.
fn anchors(p: &Policy) -> Vec<PathBuf> {
    let mut s = std::collections::BTreeSet::new();
    for x in p.protect.iter().chain(&p.deny_read) {
        s.extend(x.ancestors().skip(1).filter(|a| p.write.iter().any(|w| a.starts_with(w))).map(PathBuf::from));
    }
    s.extend(p.write.iter().cloned());
    s.into_iter().collect()
}

/// The `-D NAME=path` parameters `profile` refers to: W* write, P* protect, R* deny_read, A* anchors.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn params(p: &Policy) -> Vec<(String, PathBuf)> {
    let a = anchors(p);
    [("W", &p.write), ("P", &p.protect), ("R", &p.deny_read), ("A", &a)].into_iter()
        .flat_map(|(k, v)| v.iter().enumerate().map(move |(i, x)| (format!("{k}{i}"), x.clone())))
        .collect()
}

/// The Seatbelt profile for a policy (platform-independent, unit-tested on Linux).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn profile(p: &Policy) -> String {
    let mut s = String::from(BASE);
    match p.net {
        Net::Off => {}
        Net::Proxy { port } => {
            s += LOOPBACK;
            s += &format!("(allow network-outbound (remote ip \"localhost:{port}\"))\n{NET_SERVICES}");
        }
        Net::Open => { s += OPEN; s += NET_SERVICES; }
    }
    let mut rule = |n: usize, k: &str, r: &str| for i in 0..n { s += &r.replace('#', &format!("(param \"{k}{i}\")")); s.push('\n'); };
    rule(p.write.len(), "W", "(allow file-write* (subpath #))");
    rule(p.protect.len(), "P", "(deny file-write* (subpath #))"); // subpath covers the literal too
    rule(p.deny_read.len(), "R", "(deny file-read* file-write* (subpath #))"); // no write: no rm/mv around it
    rule(anchors(p).len(), "A", "(deny file-write-unlink (literal #))"); // rename is unlink + create
    s + TAIL
}

#[cfg(target_os = "macos")]
const EXE: &str = "/usr/bin/sandbox-exec"; // never from PATH

/// `sandbox-exec -p <profile> -D… -- <program> <args…>`. The caller sets cwd, env, stdio and runs
/// it; stdin should not be the terminal (the profile denies tty devices).
#[cfg(target_os = "macos")]
pub fn command(p: &Policy, program: &std::ffi::OsStr, args: &[&std::ffi::OsStr]) -> std::process::Command {
    let mut c = std::process::Command::new(EXE);
    c.arg("-p").arg(profile(p));
    for (k, v) in params(p) {
        let mut d = std::ffi::OsString::from(format!("-D{k}="));
        d.push(v.as_os_str());
        c.arg(d);
    }
    c.arg("--").arg(program).args(args);
    c
}

/// Some(reason) when sandboxing can't work here: no sandbox-exec, or our own base profile fails
/// to apply (already inside a sandbox, or an SBPL rule this macOS doesn't know). Cached.
#[cfg(target_os = "macos")]
pub fn unavailable() -> Option<String> {
    use std::process::Stdio;
    static U: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    U.get_or_init(|| {
        if !std::path::Path::new(EXE).is_file() { return Some(format!("{EXE} not found")) }
        let out = command(&Policy::default(), std::ffi::OsStr::new("/usr/bin/true"), &[])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output();
        match out {
            Ok(o) if o.status.success() => None,
            Ok(o) => Some(format!("sandbox-exec failed: {}", String::from_utf8_lossy(&o.stderr).trim())),
            Err(e) => Some(format!("can't run {EXE}: {e}")),
        }
    }).clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pol(net: Net) -> Policy {
        Policy {
            write: vec!["/p".into(), "/private/tmp".into()],
            deny_read: vec!["/Users/u/.ssh".into(), "/p/.env".into()],
            protect: vec!["/p/.git/hooks".into(), "/p/.git/config".into()],
            net,
        }
    }
    fn at(s: &str, needle: &str) -> usize { s.find(needle).unwrap_or_else(|| panic!("missing {needle}\n{s}")) }

    /// Parens balance outside strings, regexes and comments; every (param "X") is defined and used.
    fn well_formed(p: &Policy) {
        let (s, ps) = (profile(p), params(p));
        let (mut depth, mut in_str, mut comment) = (0i32, false, false);
        for c in s.chars() {
            match c {
                '\n' => comment = false,
                _ if comment => {}
                '"' => in_str = !in_str,
                _ if in_str => {}
                ';' => comment = true,
                '(' => depth += 1,
                ')' => { depth -= 1; assert!(depth >= 0, "unbalanced\n{s}") }
                _ => {}
            }
        }
        assert!(depth == 0 && !in_str, "unbalanced\n{s}");
        let used: Vec<&str> = s.split("(param \"").skip(1).map(|r| &r[..r.find('"').unwrap()]).collect();
        for u in &used { assert!(ps.iter().any(|(k, _)| k == u), "undefined param {u}") }
        for (k, _) in &ps { assert!(used.contains(&k.as_str()), "unused param {k}") }
    }

    #[test]
    fn order_reads_then_denies() {
        let s = profile(&pol(Net::Off));
        assert!(s.starts_with("(version 1)\n(deny default)"));
        let read = at(&s, "(allow file-read* file-test-existence)");
        assert!(at(&s, "(deny file-read* file-write* (subpath (param \"R0\")))") > read);
        assert!(at(&s, "(deny file-read* file-write* (subpath (param \"R1\")))") > read);
        let w = at(&s, "(allow file-write* (subpath (param \"W1\")))");
        assert!(at(&s, "(deny file-write* (subpath (param \"P0\")))") > w);
        assert!(at(&s, "(deny file-write* (subpath (param \"P1\")))") > w);
        assert!(at(&s, "(deny file-write-unlink (literal (param \"A0\")))") > w);
        assert!(!s.contains("(allow default)") && !s.contains("appleevent") && !s.contains("lsopen"));
        assert!(!s.contains("(allow file-write*)"), "never an unfiltered write allow");
        well_formed(&pol(Net::Off));
    }

    #[test]
    fn anchors_cover_ancestors_inside_write_roots() {
        let a: Vec<PathBuf> = params(&pol(Net::Off)).into_iter().filter(|(k, _)| k.starts_with('A')).map(|(_, v)| v).collect();
        let want: Vec<PathBuf> = ["/p", "/p/.git", "/private/tmp"].iter().map(PathBuf::from).collect();
        assert_eq!(a, want); // not /Users/u (outside every write root), not the protected paths themselves
    }

    #[test]
    fn network_variants() {
        let off = profile(&pol(Net::Off));
        assert!(!off.contains("(allow network") && !off.contains("SecurityServer"));
        let px = profile(&pol(Net::Proxy { port: 18080 }));
        assert!(px.contains("(allow network-outbound (remote ip \"localhost:18080\"))"));
        assert!(!px.contains("(allow network-outbound (remote ip \"localhost:*\"))"), "only the proxy's port");
        assert!(!px.contains("(allow network*)") && !px.contains("mDNSResponder") && !px.contains("remote ip \"*:"));
        assert!(px.contains("trustd.agent"));
        let open = profile(&pol(Net::Open));
        let all = at(&open, "(allow network*)");
        assert!(at(&open, "(deny network-outbound (remote unix-socket") > all);
        assert!(open.contains("mDNSResponder"));
        for n in [Net::Proxy { port: 1 }, Net::Open] { well_formed(&pol(n)) }
    }

    #[test]
    fn odd_paths_stay_out_of_the_profile() {
        let odd = PathBuf::from("/Users/u/my \"proj\" \\x (1);");
        let p = Policy { write: vec![odd.clone()], deny_read: vec![], protect: vec![odd.join(".qh")], net: Net::Off };
        let (s, ps) = (profile(&p), params(&p));
        assert!(!s.contains("proj"));
        assert_eq!(ps[0], ("W0".into(), odd.clone()));
        assert_eq!(ps[1], ("P0".into(), odd.join(".qh")));
        assert_eq!(ps[2], ("A0".into(), odd));
        well_formed(&p);
    }

    #[test]
    fn empty_policy_writes_nothing() {
        let p = Policy::default();
        let s = profile(&p);
        assert!(params(&p).is_empty() && !s.contains("(allow file-write*"));
        well_formed(&p);
    }
}

/// Runs the real sandbox-exec (CI on macos-latest).
#[cfg(all(test, target_os = "macos"))]
mod mac_tests {
    use super::*;
    use std::ffi::OsStr;
    use std::io::{Read, Write};
    use std::path::Path;
    use std::process::{Output, Stdio};
    use std::time::{Duration, Instant};

    struct Fx { base: PathBuf, w: PathBuf, other: PathBuf, secret: PathBuf, plain: PathBuf, hooks: PathBuf }
    impl Drop for Fx {
        fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.base); }
    }

    /// $TMPDIR/qh-sbx-<tag>-<pid>/{w/.git/hooks, other/, secret.txt, plain.txt}, canonical (/private/var/…).
    fn fx(tag: &str) -> Fx {
        let base = std::env::temp_dir().canonicalize().unwrap().join(format!("qh-sbx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (w, other) = (base.join("w"), base.join("other"));
        let hooks = w.join(".git").join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let (secret, plain) = (base.join("secret.txt"), base.join("plain.txt"));
        std::fs::write(&secret, "s3cret").unwrap();
        std::fs::write(&plain, "hello").unwrap();
        Fx { base, w, other, secret, plain, hooks }
    }

    fn pol(f: &Fx, net: Net) -> Policy {
        Policy { write: vec![f.w.clone()], deny_read: vec![f.secret.clone()], protect: vec![f.hooks.clone()], net }
    }

    /// `/bin/sh -c script` in the sandbox, killed after 30s so a hung tool can't stall CI.
    fn sh(p: &Policy, script: &str) -> Output {
        let mut ch = command(p, OsStr::new("/bin/sh"), &[OsStr::new("-c"), OsStr::new(script)])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn().expect("spawn sandbox-exec");
        let t = Instant::now();
        while ch.try_wait().expect("try_wait").is_none() {
            if t.elapsed() > Duration::from_secs(30) { let _ = ch.kill(); break; }
            std::thread::sleep(Duration::from_millis(20));
        }
        ch.wait_with_output().expect("wait")
    }

    fn q(p: &Path) -> String { format!("'{}'", p.display()) } // temp paths hold no single quote
    fn out(o: &Output) -> String { String::from_utf8_lossy(&o.stdout).into_owned() }

    #[test]
    fn available() {
        assert_eq!(unavailable(), None);
    }

    #[test]
    fn writes_inside_write_root() {
        let f = fx("write");
        let p = pol(&f, Net::Off);
        let a = f.w.join("a");
        let o = sh(&p, &format!("echo hi > {0} && mkdir -p {1}/d/e && rm -rf {1}/d && cat {0}", q(&a), q(&f.w)));
        assert!(o.status.success(), "{o:?}");
        assert_eq!(out(&o).trim(), "hi");
        assert!(!f.w.join("d").exists());
    }

    #[test]
    fn writes_outside_fail() {
        let f = fx("outside");
        let p = pol(&f, Net::Off);
        let x = f.other.join("x");
        let o = sh(&p, &format!("echo x > {}", q(&x)));
        assert!(!o.status.success() && !x.exists(), "{o:?}");
    }

    #[test]
    fn reads() {
        let f = fx("read");
        let p = pol(&f, Net::Off);
        let o = sh(&p, &format!("cat {}", q(&f.plain)));
        assert!(o.status.success() && out(&o) == "hello", "{o:?}");
        let o = sh(&p, &format!("cat {}", q(&f.secret)));
        assert!(!o.status.success() && !out(&o).contains("s3cret"), "{o:?}");
    }

    #[test]
    fn protected_paths_stay_read_only() {
        let f = fx("protect");
        let p = pol(&f, Net::Off);
        let hook = f.hooks.join("pre-commit");
        let o = sh(&p, &format!("echo evil > {}", q(&hook)));
        assert!(!o.status.success() && !hook.exists(), "{o:?}");
        // moving the parent away and back would carry hooks/ out from under its rule
        let o = sh(&p, &format!("mv {} {}", q(&f.w.join(".git")), q(&f.w.join("g2"))));
        assert!(!o.status.success() && f.hooks.is_dir(), "{o:?}");
    }

    #[test]
    fn hard_links_do_not_bypass() {
        let f = fx("link");
        let p = pol(&f, Net::Off);
        let o = sh(&p, &format!("ln {} {} ; cat {}", q(&f.secret), q(&f.w.join("k")), q(&f.w.join("k"))));
        assert!(!out(&o).contains("s3cret"), "secret read through a hard link: {o:?}");
        let hook = f.hooks.join("pre-commit");
        std::fs::write(&hook, "orig\n").unwrap();
        let h = f.w.join("h");
        let _ = sh(&p, &format!("ln {} {} ; echo evil >> {}", q(&hook), q(&h), q(&h)));
        assert_eq!(std::fs::read_to_string(&hook).unwrap(), "orig\n", "protected file written through a hard link");
    }

    #[test]
    fn dev_files_and_clone_copy() {
        let f = fx("dev");
        let p = pol(&f, Net::Off);
        let (a, b) = (f.w.join("a"), f.w.join("b"));
        // cp -c clones (APFS); Rust's fs::copy and cargo clone the same way
        let o = sh(&p, &format!("echo x > /dev/null && echo y > /dev/stderr && echo z > {} && /bin/cp -c {} {} && cat {}", q(&a), q(&a), q(&b), q(&b)));
        assert!(o.status.success() && out(&o).trim() == "z", "{o:?}");
        assert!(String::from_utf8_lossy(&o.stderr).contains('y'));
    }

    /// One-shot HTTP server on 127.0.0.1, so network tests need no internet.
    fn serve() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || for s in l.incoming().flatten() {
            let mut s = s;
            let mut b = [0u8; 1024];
            let _ = s.read(&mut b);
            let _ = s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok");
        });
        port
    }

    fn curl(url: &str) -> String { format!("/usr/bin/curl -s --noproxy '*' --max-time 5 {url}") }

    #[test]
    fn network_off_blocks_everything() {
        let f = fx("netoff");
        let p = pol(&f, Net::Off);
        let o = sh(&p, &curl("https://example.com"));
        assert!(!o.status.success(), "{o:?}");
        let o = sh(&p, &curl(&format!("http://127.0.0.1:{}/", serve())));
        assert!(!o.status.success() && out(&o) != "ok", "loopback reached under Net::Off: {o:?}");
    }

    #[test]
    fn network_proxy_allows_the_proxy_port_only() {
        let f = fx("netproxy");
        let port = serve();
        let p = pol(&f, Net::Proxy { port });
        let o = sh(&p, &curl(&format!("http://127.0.0.1:{port}/")));
        assert!(o.status.success() && out(&o) == "ok", "{o:?}");
        let other = serve();
        let o = sh(&p, &curl(&format!("http://127.0.0.1:{other}/")));
        assert!(!o.status.success(), "another local port reached: {o:?}");
        // a literal public IP needs no DNS, so failure here is the sandbox, not resolution
        let o = sh(&p, &curl("http://1.1.1.1/"));
        assert!(!o.status.success(), "{o:?}");
    }

    #[test]
    fn no_apple_events_or_launch_services() {
        let f = fx("ae");
        let p = pol(&f, Net::Off);
        // unsandboxed, `open -g -a Finder` just pokes the running Finder; sandboxed it can't reach LaunchServices
        let o = sh(&p, "/usr/bin/open -g -a Finder");
        assert!(!o.status.success(), "open escaped the sandbox: {o:?}");
        // not `get name`: AppleScript answers an application's name from its bundle without an event
        let o = sh(&p, "/usr/bin/osascript -e 'tell application \"Finder\" to count windows'");
        assert!(!o.status.success(), "AppleEvent sent: {o:?}");
    }
}
