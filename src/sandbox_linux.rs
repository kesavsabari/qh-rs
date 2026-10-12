//! The Linux sandbox for the model's commands (S2). qh runs itself again as `qh __sandbox`, which
//! 1. moves into new user, network and mount namespaces where the kernel allows unprivileged ones:
//!    only loopback exists there, and a bridge on 127.0.0.1:<port> carries connections out to the
//!    allowlist proxy over a Unix socket; protected paths are bind-mounted read-only;
//! 2. runs the command under Landlock (writes only in the policy's folders, secrets unreadable) and
//!    a seccomp filter: no new Unix sockets, which reach daemons that run things unsandboxed
//!    (systemd, tmux, docker); no io_uring, which would bypass the filter.
//!
//! Without namespaces the network is held to the proxy's port by Landlock (kernel 6.7+) and UDP is
//! refused, and qh puts protected paths back after the command (sandbox::Snapshot).
use crate::sandbox::{Net, Policy};
use serde_json::{json, Value};
use std::ffi::CString;
use std::io::Write;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// Landlock (include/uapi/linux/landlock.h)
const FS_EXECUTE: u64 = 1;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REFER: u64 = 1 << 13; // ABI 2
const FS_TRUNCATE: u64 = 1 << 14; // ABI 3
const FS_IOCTL_DEV: u64 = 1 << 15; // ABI 5
const NET_CONNECT_TCP: u64 = 1 << 1; // ABI 4
const SCOPE_ABSTRACT_UNIX: u64 = 1; // ABI 6
const SCOPE_SIGNAL: u64 = 1 << 1;
const RULE_PATH_BENEATH: libc::c_int = 1;
const RULE_NET_PORT: libc::c_int = 2;

#[repr(C)]
struct RulesetAttr {
    fs: u64,
    net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneath {
    access: u64,
    fd: i32,
}

#[repr(C)]
struct NetPort {
    access: u64,
    port: u64,
}

/// Every file-system right this kernel knows; the rights that make sense on a file (not a folder).
fn fs_rights(abi: i32) -> (u64, u64) {
    let mut all = (1u64 << 13) - 1; // ABI 1: execute … make_sym
    let mut file = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE;
    if abi >= 2 { all |= FS_REFER }
    if abi >= 3 { all |= FS_TRUNCATE; file |= FS_TRUNCATE }
    if abi >= 5 { all |= FS_IOCTL_DEV; file |= FS_IOCTL_DEV }
    (all, file)
}

/// What this kernel offers, probed once in a fresh helper (namespaces can't be tried from qh's
/// own process: it has threads).
pub struct Caps {
    /// Landlock ABI version, 0 without Landlock
    pub abi: i32,
    /// unprivileged user + network + mount namespaces work
    pub userns: bool,
}

pub fn caps() -> &'static Caps {
    static C: std::sync::OnceLock<Caps> = std::sync::OnceLock::new();
    C.get_or_init(|| {
        let out = helper().and_then(|h| Command::new(h).args(["__sandbox", "--probe"]).output().ok());
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        let get = |k: &str| text.split_whitespace().find_map(|w| w.strip_prefix(k)).and_then(|v| v.parse::<i32>().ok()).unwrap_or(0);
        Caps { abi: get("abi="), userns: get("userns=") == 1 }
    })
}

/// The qh binary to run as the helper: this one (by /proc/self/exe, which still works after
/// `qh update` replaced the file), or QH_SANDBOX_HELPER. None in unit tests: the test binary
/// isn't qh.
pub fn helper() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("QH_SANDBOX_HELPER") {
        return Some(h.into());
    }
    (!cfg!(test)).then(|| PathBuf::from("/proc/self/exe"))
}

/// `qh __sandbox <policy> -- program args…`, or None when the helper can't run here.
pub fn command(p: &Policy, sock: Option<&Path>, program: &Path, args: &[&str]) -> Option<Command> {
    let h = helper()?;
    let net = match &p.net {
        Net::Off => json!("off"),
        Net::Open => json!("open"),
        Net::Proxy { port } => json!({"port": port, "sock": sock.map(|s| s.display().to_string())}),
    };
    let paths = |v: &[PathBuf]| v.iter().map(|x| x.display().to_string()).collect::<Vec<_>>();
    // namespaces only where the probe saw them work end to end: on Ubuntu 24.04 AppArmor lets the
    // unshare itself through and then refuses what follows, leaving the helper unable to run anything
    let spec = json!({"write": paths(&p.write), "deny_read": paths(&p.deny_read), "protect": paths(&p.protect), "net": net,
                      "ns": caps().userns});
    let mut c = Command::new(h);
    c.arg("__sandbox").arg(spec.to_string()).arg("--").arg(program).args(args);
    Some(c)
}

fn errno() -> std::io::Error {
    std::io::Error::last_os_error()
}

fn landlock_abi() -> i32 {
    // LANDLOCK_CREATE_RULESET_VERSION
    let v = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, std::ptr::null::<u8>(), 0usize, 1u32) };
    v.max(0) as i32
}

fn cpath(p: &Path) -> std::io::Result<CString> {
    CString::new(p.as_os_str().as_encoded_bytes()).map_err(|_| std::io::ErrorKind::InvalidInput.into())
}

fn write_file(path: &str, body: &str) -> std::io::Result<()> {
    std::fs::OpenOptions::new().write(true).open(path)?.write_all(body.as_bytes())
}

/// New user, mount and (unless the network is open) network namespaces, mapped to our own ids.
fn unshare(net: bool) -> std::io::Result<()> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let flags = libc::CLONE_NEWUSER | libc::CLONE_NEWNS | if net { libc::CLONE_NEWNET } else { 0 };
    if unsafe { libc::unshare(flags) } != 0 {
        return Err(errno());
    }
    write_file("/proc/self/setgroups", "deny")?;
    write_file("/proc/self/uid_map", &format!("{uid} {uid} 1"))?;
    write_file("/proc/self/gid_map", &format!("{gid} {gid} 1"))?;
    // our mounts stay ours: nothing propagates back to the real system
    if unsafe { libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) } != 0 {
        return Err(errno());
    }
    if net {
        lo_up()?;
    }
    Ok(())
}

/// A new network namespace starts with loopback down.
fn lo_up() -> std::io::Result<()> {
    let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if s < 0 {
        return Err(errno());
    }
    let s = unsafe { OwnedFd::from_raw_fd(s) };
    let mut req: libc::ifreq = unsafe { std::mem::zeroed() };
    for (d, b) in req.ifr_name.iter_mut().zip(b"lo\0") {
        *d = *b as libc::c_char;
    }
    if unsafe { libc::ioctl(s.as_raw_fd(), libc::SIOCGIFFLAGS, &mut req) } != 0 {
        return Err(errno());
    }
    unsafe { req.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short };
    if unsafe { libc::ioctl(s.as_raw_fd(), libc::SIOCSIFFLAGS, &req) } != 0 {
        return Err(errno());
    }
    Ok(())
}

/// Bind `p` over itself read-only. In a user namespace the mount's locked flags (nosuid, nodev…)
/// must be repeated or the remount is refused.
fn ro_bind(p: &Path) -> std::io::Result<()> {
    let c = cpath(p)?;
    if unsafe { libc::mount(c.as_ptr(), c.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REC, std::ptr::null()) } != 0 {
        return Err(errno());
    }
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    let keep = [(libc::ST_NOSUID, libc::MS_NOSUID), (libc::ST_NODEV, libc::MS_NODEV), (libc::ST_NOEXEC, libc::MS_NOEXEC),
                (libc::ST_NOATIME, libc::MS_NOATIME), (libc::ST_NODIRATIME, libc::MS_NODIRATIME), (libc::ST_RELATIME, libc::MS_RELATIME)];
    let flags = keep.iter().filter(|(s, _)| st.f_flag & *s != 0).fold(0, |f, (_, m)| f | m);
    if unsafe { libc::mount(std::ptr::null(), c.as_ptr(), std::ptr::null(), libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | flags, std::ptr::null()) } != 0 {
        return Err(errno());
    }
    Ok(())
}

/// Inside the namespace, 127.0.0.1:port forwards each connection to the proxy's Unix socket outside.
fn bridge(port: u16, sock: PathBuf) -> std::io::Result<()> {
    let l = std::net::TcpListener::bind(("127.0.0.1", port))?;
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            let sock = sock.clone();
            std::thread::spawn(move || {
                let Ok(u) = std::os::unix::net::UnixStream::connect(&sock) else { return };
                let (Ok(mut c2), Ok(mut u2)) = (c.try_clone(), u.try_clone()) else { return };
                let up = std::thread::spawn(move || { let _ = std::io::copy(&mut c2, &mut u2); let _ = u2.shutdown(std::net::Shutdown::Write); });
                let (mut c, mut u) = (c, u);
                let _ = std::io::copy(&mut u, &mut c);
                let _ = c.shutdown(std::net::Shutdown::Write);
                let _ = up.join();
            });
        }
    });
    Ok(())
}

struct Ruleset {
    fd: OwnedFd,
    all: u64,
    file: u64,
}

impl Ruleset {
    fn new(abi: i32, connect_port: Option<Option<u16>>) -> std::io::Result<Ruleset> {
        let (all, file) = fs_rights(abi);
        let net = if abi >= 4 && connect_port.is_some() { NET_CONNECT_TCP } else { 0 };
        let attr = RulesetAttr { fs: all, net, scoped: if abi >= 6 { SCOPE_ABSTRACT_UNIX | SCOPE_SIGNAL } else { 0 } };
        let size = if abi >= 6 { 24 } else if abi >= 4 { 16 } else { 8 };
        let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr, size as usize, 0u32) };
        if fd < 0 {
            return Err(errno());
        }
        let r = Ruleset { fd: unsafe { OwnedFd::from_raw_fd(fd as i32) }, all, file };
        if let Some(Some(port)) = connect_port.filter(|_| net != 0) {
            let a = NetPort { access: NET_CONNECT_TCP, port: port as u64 };
            if unsafe { libc::syscall(libc::SYS_landlock_add_rule, r.fd.as_raw_fd(), RULE_NET_PORT, &a, 0u32) } != 0 {
                return Err(errno());
            }
        }
        Ok(r)
    }

    fn add(&self, p: &Path, access: u64) -> std::io::Result<()> {
        let c = cpath(p)?;
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(errno());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let dir = std::fs::metadata(p).is_ok_and(|m| m.is_dir());
        let a = PathBeneath { access: if dir { access & self.all } else { access & self.file }, fd: fd.as_raw_fd() };
        if unsafe { libc::syscall(libc::SYS_landlock_add_rule, self.fd.as_raw_fd(), RULE_PATH_BENEATH, &a, 0u32) } != 0 {
            return Err(errno());
        }
        Ok(())
    }

    /// Grant `access` on `dir` minus the `deny` paths beneath it: Landlock only allows, so a denied
    /// path is left out by granting each of its siblings instead, down to it. Symlinks are skipped:
    /// what they point at is governed by its own place in the tree.
    fn carve(&self, dir: &Path, access: u64, deny: &[PathBuf]) {
        if deny.iter().any(|d| d == dir) {
            return;
        }
        if !deny.iter().any(|d| d.starts_with(dir)) {
            let _ = self.add(dir, access);
            return;
        }
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                self.carve(&e.path(), access, deny);
            } else if !t.is_symlink() && !deny.contains(&e.path()) {
                let _ = self.add(&e.path(), access);
            }
        }
    }
}

/// seccomp: refuse new Unix sockets and io_uring always, and UDP/raw sockets when `udp` is false.
/// Another architecture's syscalls (32-bit binaries) are refused outright: their numbers differ.
fn seccomp_filter(udp: bool) -> Vec<libc::sock_filter> {
    #[cfg(target_arch = "x86_64")]
    const ARCH: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const ARCH: u32 = 0xC000_00B7;
    #[derive(Clone, Copy, PartialEq)]
    enum J { Next, Skip, Allow, Deny }
    use J::*;
    let (ld, ret, and) = ((libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16, (libc::BPF_RET | libc::BPF_K) as u16,
                          (libc::BPF_ALU | libc::BPF_AND | libc::BPF_K) as u16);
    let (jeq, jge) = ((libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16, (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16);
    // seccomp_data: nr @0, arch @4, args[0] @16, args[1] @24 (low halves, little-endian)
    let mut p: Vec<(u16, u32, J, J)> = vec![(ld, 4, Next, Next), (jeq, ARCH, Next, Deny), (ld, 0, Next, Next)];
    if cfg!(target_arch = "x86_64") {
        p.push((jge, 0x4000_0000, Deny, Next)); // x32: the same numbers with bit 30 set
    }
    p.extend([(jeq, libc::SYS_io_uring_setup as u32, Deny, Next), (jeq, libc::SYS_socket as u32, Next, Allow),
              (ld, 16, Next, Next), (jeq, libc::AF_UNIX as u32, Deny, Next)]);
    if !udp {
        p.extend([(jeq, libc::AF_INET as u32, Skip, Next), (jeq, libc::AF_INET6 as u32, Next, Allow),
                  (ld, 24, Next, Next), (and, 0xf, Next, Next), (jeq, libc::SOCK_STREAM as u32, Allow, Deny)]);
    }
    let (allow, deny) = (p.len(), p.len() + 1);
    let mut f: Vec<libc::sock_filter> = p.iter().enumerate().map(|(i, &(code, k, t, e))| {
        let off = |j: J| match j { Next => 0, Skip => 1, Allow => (allow - i - 1) as u8, Deny => (deny - i - 1) as u8 };
        libc::sock_filter { code, jt: off(t), jf: off(e), k }
    }).collect();
    f.push(libc::sock_filter { code: ret, jt: 0, jf: 0, k: libc::SECCOMP_RET_ALLOW });
    f.push(libc::sock_filter { code: ret, jt: 0, jf: 0, k: libc::SECCOMP_RET_ERRNO | libc::EPERM as u32 });
    f
}

/// What the helper was asked to do, parsed from its argument.
fn parse(spec: &str) -> Option<(Policy, Option<PathBuf>, bool)> {
    let v: Value = serde_json::from_str(spec).ok()?;
    let paths = |k: &str| v[k].as_array().into_iter().flatten().filter_map(|x| x.as_str().map(PathBuf::from)).collect::<Vec<_>>();
    let (net, sock) = match &v["net"] {
        Value::String(s) if s == "open" => (Net::Open, None),
        Value::Object(o) => (Net::Proxy { port: o.get("port")?.as_u64()? as u16 }, o.get("sock").and_then(|s| s.as_str()).map(PathBuf::from)),
        _ => (Net::Off, None),
    };
    Some((Policy { write: paths("write"), deny_read: paths("deny_read"), protect: paths("protect"), net }, sock, v["ns"] == true))
}

/// `qh __sandbox --probe` or `qh __sandbox <policy> -- program args…`. Fails closed: if the
/// sandbox can't be set up the command doesn't run.
pub fn helper_main(args: &[String]) -> i32 {
    let fail = |e: String| { eprintln!("qh sandbox: {e}"); 126 };
    if args.first().map(String::as_str) == Some("--probe") {
        let abi = landlock_abi();
        let ns = unshare(true).is_ok();
        println!("abi={abi} userns={}", ns as i32);
        return 0;
    }
    let (Some(spec), Some(dash)) = (args.first(), args.iter().position(|a| a == "--")) else { return fail("usage".into()) };
    let Some((p, sock, try_ns)) = parse(spec) else { return fail("bad policy".into()) };
    let Some(prog) = args.get(dash + 1) else { return fail("no command".into()) };
    let abi = landlock_abi();
    if abi < 1 {
        return fail("this kernel has no Landlock (5.13+ with landlock in lsm=)".into());
    }
    // QH_SANDBOX_NO_NS=1: behave as on a system without unprivileged namespaces (tests, CI)
    let ns = try_ns && std::env::var("QH_SANDBOX_NO_NS").as_deref() != Ok("1");
    if ns {
        if let Err(e) = unshare(p.net != Net::Open) {
            return fail(format!("namespaces: {e}")); // half set up: don't run anything
        }
    }
    if ns {
        for x in p.protect.iter().filter(|x| x.exists()) {
            if let Err(e) = ro_bind(x) {
                return fail(format!("can't protect {}: {e}", x.display()));
            }
        }
        if let (Net::Proxy { port }, Some(sock)) = (&p.net, sock) {
            if let Err(e) = bridge(*port, sock) {
                return fail(format!("network bridge: {e}"));
            }
        }
    }
    // without a network namespace Landlock holds TCP to the proxy's port (or nothing)
    let connect = match (&p.net, ns) {
        (Net::Open, _) | (_, true) => None,
        (Net::Proxy { port }, false) => Some(Some(*port)),
        (Net::Off, false) => Some(None),
    };
    let rs = match Ruleset::new(abi, connect) {
        Ok(r) => r,
        Err(e) => return fail(format!("landlock: {e}")),
    };
    let read = FS_EXECUTE | FS_READ_FILE | FS_READ_DIR | FS_IOCTL_DEV;
    rs.carve(Path::new("/"), read, &p.deny_read);
    for w in &p.write {
        rs.carve(w, rs.all, &p.deny_read);
    }
    let _ = rs.add(Path::new("/dev"), read | FS_WRITE_FILE | FS_TRUNCATE);
    let filter = seccomp_filter(ns || p.net == Net::Open);
    let fd = rs.fd.as_raw_fd();
    let mut cmd = Command::new(prog);
    cmd.args(&args[dash + 2..]);
    // runs in the forked child just before exec: only syscalls, no allocation
    unsafe {
        cmd.pre_exec(move || {
            let prog = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr() as *mut _ };
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
                || libc::syscall(libc::SYS_landlock_restrict_self, fd, 0u32) != 0
                || libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &prog as *const libc::sock_fprog) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    match cmd.status() {
        Ok(s) => {
            use std::os::unix::process::ExitStatusExt;
            s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0))
        }
        Err(e) => fail(format!("{prog}: {e}")),
    }
}

/// For /sandbox: what's enforced on this machine.
pub fn describe() -> String {
    let c = caps();
    match (c.abi, c.userns) {
        (0, _) => "not enforced: this kernel has no Landlock (needs Linux 5.13+ with landlock in the lsm= list)".into(),
        (a, true) => format!("Landlock ABI {a} + own network and mount namespaces + seccomp"),
        (a, false) if a >= 4 => format!("Landlock ABI {a} + seccomp; no unprivileged namespaces, so the network is held to the proxy's port and protected paths are restored after each command"),
        (a, false) => format!("Landlock ABI {a} + seccomp; no unprivileged namespaces and Landlock before ABI 4, so TCP isn't confined (only UDP is blocked)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_round_trips_through_the_helper_argument() {
        let p = Policy { write: vec!["/w".into()], deny_read: vec!["/h/.ssh".into()], protect: vec!["/w/.git/hooks".into()], net: Net::Proxy { port: 4321 } };
        std::env::set_var("QH_SANDBOX_HELPER", "/bin/true");
        let c = command(&p, Some(Path::new("/tmp/s.sock")), Path::new("bash"), &["-c", "ls"]).unwrap();
        let args: Vec<String> = c.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        let (q, sock, _) = parse(&args[1]).unwrap();
        assert_eq!((q.write, q.deny_read, q.protect, q.net), (p.write, p.deny_read, p.protect, p.net));
        assert_eq!(sock, Some(PathBuf::from("/tmp/s.sock")));
        assert_eq!(&args[2..], ["--", "bash", "-c", "ls"]);
    }

    #[test]
    fn seccomp_jumps_stay_in_the_program() {
        for udp in [true, false] {
            let f = seccomp_filter(udp);
            for (i, ins) in f.iter().enumerate() {
                if ins.code as u32 & libc::BPF_JMP != 0 && ins.code as u32 & 0x07 == libc::BPF_JMP {
                    assert!(i + 1 + (ins.jt.max(ins.jf) as usize) < f.len(), "jump out of range at {i}");
                }
            }
            assert_eq!(f[f.len() - 2].k, libc::SECCOMP_RET_ALLOW);
        }
    }
}
