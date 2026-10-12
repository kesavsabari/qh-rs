//! Native-Windows sandbox for the model's shell commands (safety plan S4, AppContainer spike).
//!
//! Commands run in one AppContainer ("qh.sandbox"). No admin needed, and the OS does the confining:
//! the container can only open objects whose DACL names its SID (or ALL APPLICATION PACKAGES, which
//! Windows gives to System32 and Program Files), so the user's home, ~/.ssh included, stays unreadable
//! unless granted; and without capability SIDs it has no network at all.
//! Grants are ordinary ACEs for the container SID, added once per path and recorded for `reset()`.
//! The ACE handling (check-before-set, deny ACEs via SetEntriesInAclW) and the attribute-list wrapper
//! follow ideas from Codex's codex-rs/windows-sandbox-rs (Apache-2.0); no code is vendored.
//!
//! Only the pure helpers (quoting, environment block, choosing what to grant) build on other systems,
//! for their unit tests.
#![cfg_attr(not(windows), allow(dead_code))]
use crate::sandbox::Policy;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// What one ACE does for the container. Ordered so allows are applied before the denies that carve
/// exceptions out of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Kind {
    /// modify (read, write, run, delete) — but not WRITE_DAC, so commands can't edit our ACEs away
    Write,
    /// read and run: toolchains under the profile
    Read,
    /// a secret inside something granted
    DenyRead,
    /// a `protect` path inside a writable folder
    DenyWrite,
    /// a folder between a `protect` path and its writable root: renaming it would swap the protected
    /// path for a fresh one
    DenyDelete,
}

fn wide(s: &OsStr) -> Vec<u16> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        s.encode_wide().collect()
    }
    #[cfg(not(windows))]
    {
        s.to_string_lossy().encode_utf16().collect()
    }
}

const BS: u16 = b'\\' as u16;
const QUOTE: u16 = b'"' as u16;

/// One argument, quoted the way the MS C runtime splits command lines (the same rules as Rust's std):
/// backslashes are literal unless they precede a quote; then they double, and the quote is escaped.
fn quote_arg(v: &mut Vec<u16>, a: &[u16]) {
    let q = a.is_empty() || a.iter().any(|&c| matches!(c, 0x20 | 0x09 | 0x0a | 0x0b));
    if q { v.push(QUOTE) }
    let mut bs = 0;
    for &c in a {
        if c == BS {
            bs += 1;
        } else {
            if c == QUOTE { v.extend(std::iter::repeat_n(BS, bs + 1)) }
            bs = 0;
        }
        v.push(c);
    }
    if q {
        v.extend(std::iter::repeat_n(BS, bs));
        v.push(QUOTE);
    }
}

/// `program args…` as a NUL-terminated CreateProcessW command line. argv[0] is always quoted and never
/// escaped (the CRT reads it up to the next quote). cmd.exe doesn't follow the CRT rules: keep `"` out
/// of `cmd /c` scripts.
fn command_line(program: &OsStr, args: &[OsString]) -> Vec<u16> {
    let mut v = vec![QUOTE];
    v.extend(wide(program));
    v.push(QUOTE);
    for a in args {
        v.push(b' ' as u16);
        quote_arg(&mut v, &wide(a));
    }
    v.push(0);
    v
}

/// The environment as CREATE_UNICODE_ENVIRONMENT wants it: `k=v\0` entries sorted by name ignoring
/// case, then one more NUL. A later duplicate (in any case) wins; names with `=` past the first char
/// (`=C:` is legal) or NULs are dropped.
fn env_block(env: &[(OsString, OsString)]) -> Vec<u16> {
    let up = |k: &[u16]| k.iter().map(|&c| if (0x61..=0x7a).contains(&c) { c - 32 } else { c }).collect::<Vec<u16>>();
    let mut m = std::collections::BTreeMap::new();
    for (k, v) in env {
        let (k, v) = (wide(k), wide(v));
        if k.is_empty() || k[1..].contains(&(b'=' as u16)) || k.contains(&0) || v.contains(&0) { continue }
        m.insert(up(&k), (k, v));
    }
    let mut b = vec![];
    for (k, v) in m.into_values() {
        b.extend(k);
        b.push(b'=' as u16);
        b.extend(v);
        b.push(0);
    }
    if b.is_empty() { b.push(0) }
    b.push(0);
    b
}

/// Drop the `\\?\` that canonicalize() adds: cmd.exe refuses it as a working folder.
fn plain(p: &Path) -> PathBuf {
    match p.to_str() {
        Some(s) if s.starts_with(r"\\?\UNC\") => PathBuf::from(format!(r"\\{}", &s[8..])),
        Some(s) if s.starts_with(r"\\?\") && s.as_bytes().get(5) == Some(&b':') => PathBuf::from(&s[4..]),
        _ => p.to_path_buf(),
    }
}

/// For comparing paths: lowercase (NTFS ignores case), `\`-separated, no `\\?\`, no trailing `\`.
fn norm(p: &Path) -> String {
    plain(p).to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_lowercase()
}

/// `p` is `root` or inside it.
fn under(p: &Path, root: &Path) -> bool {
    let (p, r) = (norm(p), norm(root));
    p == r || p.starts_with(&(r + "\\"))
}

/// Toolchains under the profile that commands may read and run (Python/Node/Git installs, cargo,
/// rustup, scoop shims). Never the profile itself, .ssh, .aws, or AppData\Roaming as a whole.
const TOOLCHAIN: &[&str] = &[".cargo", ".rustup", ".gitconfig", "scoop", ".pyenv", "AppData/Local/Programs",
    "AppData/Roaming/npm", "AppData/Roaming/nvm", "AppData/Local/Volta"];

/// Every ACE a command needs as (kind, path, is_dir), allows first. Only existing paths; nothing in or
/// under `deny_read` is granted, and a secret inside a granted folder gets a deny of its own.
fn plan(p: &Policy, home: &Path) -> Vec<(Kind, PathBuf, bool)> {
    let denied = |x: &Path| p.deny_read.iter().any(|d| under(x, d));
    let mut v = vec![];
    for w in &p.write {
        if w.is_dir() && !denied(w) { v.push((Kind::Write, w.clone(), true)) }
    }
    for t in TOOLCHAIN {
        let path = t.split('/').fold(home.to_path_buf(), |a, c| a.join(c));
        if let Ok(m) = path.metadata() {
            if !denied(&path) { v.push((Kind::Read, path, m.is_dir())) }
        }
    }
    let granted: Vec<PathBuf> = v.iter().map(|g| g.1.clone()).collect();
    for d in &p.deny_read {
        if let Ok(m) = d.metadata() {
            if granted.iter().any(|g| under(d, g)) { v.push((Kind::DenyRead, d.clone(), m.is_dir())) }
        }
    }
    for pr in &p.protect {
        let Ok(m) = pr.metadata() else { continue };
        // the innermost writable folder holding it; none means it's read-only anyway
        let Some(root) = p.write.iter().filter(|w| under(pr, w)).max_by_key(|w| norm(w).len()) else { continue };
        v.push((Kind::DenyWrite, pr.clone(), m.is_dir()));
        let mut a = pr.parent();
        while let Some(x) = a.filter(|x| under(x, root) && norm(x) != norm(root)) {
            v.push((Kind::DenyDelete, x.to_path_buf(), true));
            a = x.parent();
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    v.retain(|(k, path, _)| seen.insert((*k, norm(path))));
    v
}

#[cfg(windows)]
pub use win::{reset, run_timeout, unavailable};

#[cfg(windows)]
mod win {
    use super::*;
    use crate::sandbox::Net;
    use std::ffi::c_void;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::mem::{size_of_val, zeroed};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::ExitStatusExt;
    use std::process::{ExitStatus, Output};
    use std::ptr::{self, null, null_mut};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};
    use windows_sys::core::{HRESULT, PWSTR};
    use windows_sys::Win32::Foundation::{LocalFree, SetHandleInformation, ERROR_ALREADY_EXISTS,
        ERROR_SUCCESS, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, S_OK, WAIT_OBJECT_0};
    use windows_sys::Win32::Security::Authorization::{ConvertStringSidToSidW, GetNamedSecurityInfoW,
        SetEntriesInAclW, SetNamedSecurityInfoW, DENY_ACCESS, EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE,
        SE_FILE_OBJECT, TRUSTEE_IS_SID, TRUSTEE_IS_UNKNOWN, TRUSTEE_W};
    use windows_sys::Win32::Security::Isolation::{CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName};
    use windows_sys::Win32::Security::{AddAce, EqualSid, FreeSid, GetAce, InitializeAcl, ACCESS_ALLOWED_ACE, ACE_HEADER,
        ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, INHERITED_ACE, INHERIT_ONLY_ACE, NO_INHERITANCE,
        OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_EXECUTE,
        FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_DATA, FILE_READ_EA,
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER};
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
        JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE};
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{CreateProcessW, DeleteProcThreadAttributeList, GetExitCodeProcess,
        InitializeProcThreadAttributeList, ResumeThread, TerminateProcess, UpdateProcThreadAttribute, WaitForSingleObject,
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
        LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, STARTF_USESTDHANDLES, STARTUPINFOEXW};

    const NAME: &str = "qh.sandbox";
    // winnt.h; kept local so windows-sys needn't pull in Win32_System_SystemServices
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;
    const SE_GROUP_ENABLED: u32 = 4;
    const HR_ALREADY_EXISTS: HRESULT = (0x8007_0000 | ERROR_ALREADY_EXISTS) as HRESULT; // HRESULT_FROM_WIN32

    const MODIFY: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE;
    const READ_RUN: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
    const NO_READ: u32 = FILE_READ_DATA | FILE_READ_EA | FILE_EXECUTE;
    const NO_WRITE: u32 = FILE_WRITE_DATA | FILE_APPEND_DATA | FILE_WRITE_EA | FILE_WRITE_ATTRIBUTES | DELETE
        | FILE_DELETE_CHILD | WRITE_DAC | WRITE_OWNER;

    fn wz(s: impl AsRef<OsStr>) -> Vec<u16> {
        let mut v = wide(s.as_ref());
        v.push(0);
        v
    }

    fn last_err() -> io::Error {
        io::Error::last_os_error()
    }

    fn hr_err(what: &str, hr: HRESULT) -> io::Error {
        io::Error::other(format!("{what} failed: HRESULT 0x{:08X}", hr as u32))
    }

    /// A SID from CreateAppContainerProfile / DeriveAppContainerSidFromAppContainerName.
    pub(super) struct Sid(pub(super) PSID);
    impl Drop for Sid {
        fn drop(&mut self) {
            unsafe { FreeSid(self.0) };
        }
    }

    /// Memory the system LocalAlloc'd for us (security descriptors, ACLs, string SIDs).
    struct Local(*mut c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            if !self.0.is_null() { unsafe { LocalFree(self.0) }; }
        }
    }

    /// The container's SID, making the profile the first time.
    fn container_sid() -> io::Result<Sid> {
        let (name, display) = (wz(NAME), wz("qh sandbox"));
        let mut sid: PSID = null_mut();
        // SAFETY: NUL-terminated strings; no capabilities; on success `sid` is ours to FreeSid
        let hr = unsafe { CreateAppContainerProfile(name.as_ptr(), display.as_ptr(), display.as_ptr(), null(), 0, &mut sid) };
        match hr {
            S_OK => Ok(Sid(sid)),
            HR_ALREADY_EXISTS => derive_sid(),
            _ => Err(hr_err("CreateAppContainerProfile", hr)),
        }
    }

    /// The SID for our name; it's a hash of the name, so this works with or without the profile.
    fn derive_sid() -> io::Result<Sid> {
        let name = wz(NAME);
        let mut sid: PSID = null_mut();
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        if hr == S_OK { Ok(Sid(sid)) } else { Err(hr_err("DeriveAppContainerSidFromAppContainerName", hr)) }
    }

    /// Capability SIDs for `net`. Net::Proxy gets none: AppContainer loopback isolation blocks
    /// 127.0.0.1 too (only a loopback exemption, which needs admin, would open it), so on Windows the
    /// proxy is unreachable and Proxy means the same as Off.
    fn caps(net: &Net) -> io::Result<Vec<Local>> {
        if *net != Net::Open { return Ok(vec![]) }
        // internetClient, privateNetworkClientServer (well-known capability SIDs)
        ["S-1-15-3-1", "S-1-15-3-3"].iter().map(|s| {
            let (w, mut sid) = (wz(s), null_mut());
            // SAFETY: NUL-terminated string; the SID is LocalFree'd by `Local`
            if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 { return Err(last_err()) }
            Ok(Local(sid))
        }).collect()
    }

    /// The DACL of `path`; the pointer lives as long as the returned descriptor.
    fn read_dacl(path: &Path) -> io::Result<(Local, *mut ACL)> {
        let w = wz(plain(path));
        let (mut dacl, mut sd): (*mut ACL, PSECURITY_DESCRIPTOR) = (null_mut(), null_mut());
        // SAFETY: out-pointers are valid; `sd` owns the memory `dacl` points into
        let e = unsafe { GetNamedSecurityInfoW(w.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, null_mut(),
            null_mut(), &mut dacl, null_mut(), &mut sd) };
        if e != ERROR_SUCCESS { return Err(io::Error::from_raw_os_error(e as i32)) }
        Ok((Local(sd), dacl))
    }

    fn write_dacl(path: &Path, dacl: *const ACL) -> io::Result<()> {
        let w = wz(plain(path));
        // propagates inheritable ACEs to everything below: slow on big trees, hence once per path
        let e = unsafe { SetNamedSecurityInfoW(w.as_ptr(), SE_FILE_OBJECT, DACL_SECURITY_INFORMATION, null_mut(),
            null_mut(), dacl, null()) };
        if e != ERROR_SUCCESS { return Err(io::Error::from_raw_os_error(e as i32)) }
        Ok(())
    }

    /// The SID inside an ACCESS_ALLOWED/DENIED ACE (same layout for both).
    unsafe fn ace_sid(ace: *mut c_void) -> PSID {
        ptr::addr_of!((*(ace as *const ACCESS_ALLOWED_ACE)).SidStart) as PSID
    }

    /// Does `dacl` already hold an ACE of type `ty` for `sid` covering `mask` and `flags`?
    /// Inherited ones count; inherit-only ones don't apply to the object itself.
    pub(super) unsafe fn has_ace(dacl: *const ACL, sid: PSID, ty: u8, mask: u32, flags: u32) -> bool {
        if dacl.is_null() { return false }
        for i in 0..(*dacl).AceCount as u32 {
            let mut ace = null_mut();
            if GetAce(dacl, i, &mut ace) == 0 { continue }
            let h = &*(ace as *const ACE_HEADER);
            let f = h.AceFlags as u32;
            if h.AceType == ty && f & INHERIT_ONLY_ACE == 0 && f & flags == flags
                && (*(ace as *const ACCESS_ALLOWED_ACE)).Mask & mask == mask && EqualSid(ace_sid(ace), sid) != 0 {
                return true;
            }
        }
        false
    }

    /// Add one ACE for `sid` on `path` unless an equal one is there. Records the path before touching it.
    fn add_ace(path: &Path, sid: PSID, k: Kind, dir: bool) -> io::Result<()> {
        let (mask, mode, ty) = match k {
            Kind::Write => (MODIFY, GRANT_ACCESS, ACCESS_ALLOWED_ACE_TYPE),
            Kind::Read => (READ_RUN, GRANT_ACCESS, ACCESS_ALLOWED_ACE_TYPE),
            Kind::DenyRead => (NO_READ, DENY_ACCESS, ACCESS_DENIED_ACE_TYPE),
            Kind::DenyWrite => (NO_WRITE, DENY_ACCESS, ACCESS_DENIED_ACE_TYPE),
            Kind::DenyDelete => (DELETE, DENY_ACCESS, ACCESS_DENIED_ACE_TYPE),
        };
        let flags = if dir && k != Kind::DenyDelete { OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE } else { NO_INHERITANCE };
        let (_sd, dacl) = read_dacl(path)?;
        if unsafe { has_ace(dacl, sid, ty, mask, flags) } { return Ok(()) }
        record(path)?;
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask,
            grfAccessMode: mode,
            grfInheritance: flags,
            Trustee: TRUSTEE_W { pMultipleTrustee: null_mut(), MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID, TrusteeType: TRUSTEE_IS_UNKNOWN, ptstrName: sid as PWSTR },
        };
        let mut new: *mut ACL = null_mut();
        // SetEntriesInAclW keeps canonical order: a new deny lands ahead of the allows, so it wins
        let e = unsafe { SetEntriesInAclW(1, &ea, dacl, &mut new) };
        if e != ERROR_SUCCESS { return Err(io::Error::from_raw_os_error(e as i32)) }
        let _new = Local(new.cast());
        write_dacl(path, new)
    }

    fn grants_file() -> PathBuf {
        crate::platform::state_base().join("sandbox-grants.txt")
    }

    fn record(path: &Path) -> io::Result<()> {
        let f = grants_file();
        if let Some(d) = f.parent() { fs::create_dir_all(d)? }
        let mut o = fs::OpenOptions::new().create(true).append(true).open(f)?;
        writeln!(o, "{}", plain(path).display())
    }

    /// (kind, normalized path) already in place this run: skips even the DACL read.
    static DONE: Mutex<std::collections::BTreeSet<(Kind, String)>> = Mutex::new(std::collections::BTreeSet::new());

    fn grant(p: &Policy, sid: PSID) -> io::Result<()> {
        let home = std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(crate::platform::home);
        let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
        for (k, path, dir) in plan(p, &home) {
            let key = (k, norm(&path));
            if done.contains(&key) { continue }
            match add_ace(&path, sid, k, dir) {
                Ok(()) => { done.insert(key); }
                Err(_) if k == Kind::Read => {} // best effort: that tool just won't run inside
                // a deny that didn't land must not run the command; a failed write grant would only confuse
                Err(e) => return Err(io::Error::new(e.kind(), format!("sandbox: can't set access on {}: {e}", path.display()))),
            }
        }
        Ok(())
    }

    /// Run `program args…` in the AppContainer with `env` (the complete environment block) in `cwd`,
    /// stdin closed, stdout/stderr captured; kill the whole process tree after `secs`.
    /// Returns (output, killed) like crate::platform::run_timeout. `cwd` must be inside a grant.
    pub fn run_timeout(p: &Policy, program: &Path, args: &[OsString], env: &[(OsString, OsString)], secs: u64,
                       cwd: &Path) -> io::Result<(Output, bool)> {
        let sid = container_sid()?;
        grant(p, sid.0)?;
        spawn(&p.net, sid.0, program, args, env, secs, cwd)
    }

    /// Some(reason) when the AppContainer can't be used here; None when it works. Probed once.
    pub fn unavailable() -> Option<String> {
        static U: OnceLock<Option<String>> = OnceLock::new();
        U.get_or_init(|| {
            // the spike's answer (CI, 2026-10-12): Git Bash, qh's shell on Windows, dies at start in an
            // AppContainer (0xc0000142) and PowerShell loses its cmdlets, so it stays opt-in for now
            if std::env::var("QH_SANDBOX_APPCONTAINER").as_deref() != Ok("1") {
                return Some("not yet on Windows: commands run unsandboxed (the secret, write and environment guards still \
apply). An AppContainer can't run Git Bash; sandbox users are planned.".into());
            }
            let sys = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join("System32");
            let env: Vec<_> = std::env::vars_os().collect();
            let args = ["/c".into(), "exit 0".into()];
            // no grants: System32 is open to every AppContainer
            match container_sid().and_then(|sid| spawn(&Net::Off, sid.0, &sys.join("cmd.exe"), &args, &env, 30, &sys)) {
                Ok((o, false)) if o.status.success() => None,
                Ok((o, killed)) => Some(format!("AppContainer probe {}: {}", if killed { "timed out".into() } else { o.status.to_string() },
                    String::from_utf8_lossy(&o.stderr).trim())),
                Err(e) => Some(format!("AppContainer unavailable: {e}")),
            }
        }).clone()
    }

    /// Remove every ACE qh added (for `qh sandbox reset`); returns how many paths were cleaned.
    /// Paths that fail stay listed so a later reset can retry them.
    pub fn reset() -> io::Result<usize> {
        let f = grants_file();
        let text = match fs::read_to_string(&f) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let sid = derive_sid()?;
        let (mut seen, mut n, mut left) = (std::collections::BTreeSet::new(), 0, String::new());
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
            if !seen.insert(line.to_lowercase()) { continue }
            match strip(Path::new(line), sid.0) {
                Ok(true) => n += 1,
                Ok(false) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(_) => { left += line; left.push('\n') }
            }
        }
        if left.is_empty() { fs::remove_file(&f)? } else { fs::write(&f, left)? }
        DONE.lock().unwrap_or_else(|e| e.into_inner()).clear();
        Ok(n)
    }

    /// Rewrite `path`'s DACL without the explicit ACEs for `sid`; Ok(false) when there were none.
    fn strip(path: &Path, sid: PSID) -> io::Result<bool> {
        let (_sd, dacl) = read_dacl(path)?;
        if dacl.is_null() { return Ok(false) }
        // SAFETY: `dacl` is a valid ACL kept alive by `_sd`; `buf` is u64-aligned and as big as the old ACL
        unsafe {
            let (size, rev) = ((*dacl).AclSize as u32, (*dacl).AclRevision as u32);
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let new = buf.as_mut_ptr() as *mut ACL;
            if InitializeAcl(new, size, rev) == 0 { return Err(last_err()) }
            let mut dropped = false;
            for i in 0..(*dacl).AceCount as u32 {
                let mut ace = null_mut();
                if GetAce(dacl, i, &mut ace) == 0 { return Err(last_err()) }
                let h = &*(ace as *const ACE_HEADER);
                if matches!(h.AceType, ACCESS_ALLOWED_ACE_TYPE | ACCESS_DENIED_ACE_TYPE)
                    && h.AceFlags as u32 & INHERITED_ACE == 0 && EqualSid(ace_sid(ace), sid) != 0 {
                    dropped = true;
                    continue;
                }
                if AddAce(new, rev, u32::MAX, ace, h.AceSize as u32) == 0 { return Err(last_err()) }
            }
            if !dropped { return Ok(false) }
            write_dacl(path, new)?;
        }
        Ok(true)
    }

    /// A PROC_THREAD_ATTRIBUTE_LIST in a pointer-aligned buffer.
    struct AttrList(Vec<usize>);
    impl AttrList {
        fn new(n: u32) -> io::Result<Self> {
            let mut size = 0;
            // SAFETY: a null list only asks for the size (that call reports ERROR_INSUFFICIENT_BUFFER)
            unsafe { InitializeProcThreadAttributeList(null_mut(), n, 0, &mut size) };
            let mut buf = vec![0usize; size.div_ceil(size_of::<usize>()).max(1)];
            if unsafe { InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), n, 0, &mut size) } == 0 {
                return Err(last_err());
            }
            Ok(AttrList(buf))
        }
        fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
            self.0.as_mut_ptr().cast()
        }
        /// SAFETY: `v` (n bytes) must stay alive and unmoved until CreateProcessW returns.
        unsafe fn set(&mut self, attr: u32, v: *const c_void, n: usize) -> io::Result<()> {
            if UpdateProcThreadAttribute(self.ptr(), 0, attr as usize, v, n, null_mut(), null()) == 0 { return Err(last_err()) }
            Ok(())
        }
    }
    impl Drop for AttrList {
        fn drop(&mut self) {
            unsafe { DeleteProcThreadAttributeList(self.ptr()) };
        }
    }

    fn owned(h: HANDLE) -> io::Result<OwnedHandle> {
        if h.is_null() || h == INVALID_HANDLE_VALUE { return Err(last_err()) }
        Ok(unsafe { OwnedHandle::from_raw_handle(h) })
    }

    /// An anonymous pipe as (read, write); neither end inheritable yet.
    fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
        let (mut r, mut w) = (null_mut(), null_mut());
        if unsafe { CreatePipe(&mut r, &mut w, null(), 0) } == 0 { return Err(last_err()) }
        Ok(unsafe { (OwnedHandle::from_raw_handle(r), OwnedHandle::from_raw_handle(w)) })
    }

    /// Jobs whose processes outlived their command (background jobs). Closing a job kills what's in it
    /// (KILL_ON_JOB_CLOSE), so they live until qh exits, like the process groups on Linux mostly do.
    static KEEP: Mutex<Vec<OwnedHandle>> = Mutex::new(Vec::new());

    fn drain(f: fs::File) -> std::sync::mpsc::Receiver<Option<Vec<u8>>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut f, mut buf) = (f, [0u8; 8192]);
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 { break }
                let _ = tx.send(Some(buf[..n].to_vec()));
            }
            let _ = tx.send(None);
        });
        rx
    }

    fn spawn(net: &Net, sid: PSID, program: &Path, args: &[OsString], env: &[(OsString, OsString)], secs: u64,
             cwd: &Path) -> io::Result<(Output, bool)> {
        let caps = caps(net)?;
        let mut sa: Vec<SID_AND_ATTRIBUTES> = caps.iter().map(|c| SID_AND_ATTRIBUTES { Sid: c.0, Attributes: SE_GROUP_ENABLED }).collect();
        let sc = SECURITY_CAPABILITIES { AppContainerSid: sid, Capabilities: if sa.is_empty() { null_mut() } else { sa.as_mut_ptr() },
            CapabilityCount: sa.len() as u32, Reserved: 0 };
        let (out_r, out_w) = pipe()?;
        let (err_r, err_w) = pipe()?;
        let nul: OwnedHandle = fs::File::open("NUL")?.into(); // stdin: EOF at once
        let inherit: [HANDLE; 3] = [nul.as_raw_handle(), out_w.as_raw_handle(), err_w.as_raw_handle()];

        // the job exists before the process, which starts suspended, so no child can escape it
        let job = owned(unsafe { CreateJobObjectW(null(), null()) })?;
        let mut lim: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        lim.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe { SetInformationJobObject(job.as_raw_handle(), JobObjectExtendedLimitInformation,
            ptr::from_ref(&lim).cast(), size_of_val(&lim) as u32) } == 0 { return Err(last_err()) }

        let mut attrs = AttrList::new(2)?;
        // SAFETY: `sc`, `sa` (via sc.Capabilities) and `inherit` outlive CreateProcessW below
        unsafe {
            attrs.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, ptr::from_ref(&sc).cast(), size_of_val(&sc))?;
            // only these three handles reach the child, whatever else is inheritable in qh right now
            attrs.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, inherit.as_ptr().cast(), size_of_val(&inherit))?;
        }
        for h in inherit {
            if unsafe { SetHandleInformation(h, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 { return Err(last_err()) }
        }
        let mut si: STARTUPINFOEXW = unsafe { zeroed() };
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        (si.StartupInfo.hStdInput, si.StartupInfo.hStdOutput, si.StartupInfo.hStdError) = (inherit[0], inherit[1], inherit[2]);
        si.lpAttributeList = attrs.ptr();
        let prog = plain(program);
        let mut cmdline = command_line(prog.as_os_str(), args);
        let app = prog.is_absolute().then(|| wz(&prog)); // else CreateProcessW searches for it
        let (envb, dir) = (env_block(env), wz(plain(cwd)));
        let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
        // SAFETY: all pointers are to live, NUL-terminated buffers; `si` is a STARTUPINFOEXW as
        // EXTENDED_STARTUPINFO_PRESENT promises
        let ok = unsafe { CreateProcessW(app.as_ref().map_or(null(), |a| a.as_ptr()), cmdline.as_mut_ptr(), null(), null(), 1,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW | CREATE_SUSPENDED,
            envb.as_ptr().cast(), dir.as_ptr(), &si.StartupInfo, &mut pi) };
        let err = last_err();
        // the child has its own copies now; ours would keep the pipes open forever
        drop((attrs, nul, out_w, err_w));
        if ok == 0 { return Err(err) }
        let (process, thread) = unsafe { (OwnedHandle::from_raw_handle(pi.hProcess), OwnedHandle::from_raw_handle(pi.hThread)) };
        let ph = process.as_raw_handle();
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), ph) } == 0 || unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            let e = last_err();
            unsafe { TerminateProcess(ph, 1) };
            return Err(e);
        }
        let (out, errs) = (drain(fs::File::from(out_r)), drain(fs::File::from(err_r)));

        let ms = secs.saturating_mul(1000).min(u32::MAX as u64 - 1) as u32; // u32::MAX is INFINITE
        let killed = unsafe { WaitForSingleObject(ph, ms) } != WAIT_OBJECT_0;
        if killed {
            unsafe {
                TerminateJobObject(job.as_raw_handle(), 1);
                WaitForSingleObject(ph, 5000);
            }
        }
        let mut code = 1u32;
        unsafe { GetExitCodeProcess(ph, &mut code) };
        if !killed {
            let mut acct: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { zeroed() };
            let ok = unsafe { QueryInformationJobObject(job.as_raw_handle(), JobObjectBasicAccountingInformation,
                ptr::from_mut(&mut acct).cast(), size_of_val(&acct) as u32, null_mut()) };
            if ok != 0 && acct.ActiveProcesses > 0 { KEEP.lock().unwrap_or_else(|e| e.into_inner()).push(job) }
        }
        // a background process left running may hold the pipe open: take what's there and go
        let collect = |rx: std::sync::mpsc::Receiver<Option<Vec<u8>>>| {
            let mut v = vec![];
            let end = Instant::now() + Duration::from_millis(if killed { 300 } else { 2000 });
            while let Ok(Some(chunk)) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                v.extend(chunk);
            }
            v
        };
        Ok((Output { status: ExitStatus::from_raw(code), stdout: collect(out), stderr: collect(errs) }, killed))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::Command;

        /// grants and reset() are machine-wide: one test at a time
        static SERIAL: Mutex<()> = Mutex::new(());

        fn sys(exe: &str) -> PathBuf {
            PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())).join("System32").join(exe)
        }

        /// …\qh-sbx-<pid>-<tag>\{a,b}, canonical like the real policy's paths
        fn dirs(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
            let root = std::env::temp_dir().join(format!("qh-sbx-{}-{tag}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("a")).unwrap();
            fs::create_dir_all(root.join("b")).unwrap();
            let root = fs::canonicalize(root).unwrap();
            (root.clone(), root.join("a"), root.join("b"))
        }

        /// the caller's environment with TEMP/TMP pointed at `tmp` (the real %TEMP% isn't granted)
        fn env_with_tmp(tmp: &Path) -> Vec<(OsString, OsString)> {
            let mut e: Vec<_> = std::env::vars_os().filter(|(k, _)| !k.eq_ignore_ascii_case("TEMP") && !k.eq_ignore_ascii_case("TMP")).collect();
            e.push(("TEMP".into(), plain(tmp).into()));
            e.push(("TMP".into(), plain(tmp).into()));
            e
        }

        fn cmd(p: &Policy, script: &str, cwd: &Path, secs: u64) -> (Output, bool) {
            run_timeout(p, &sys("cmd.exe"), &["/c".into(), script.into()], &env_with_tmp(cwd), secs, cwd).unwrap()
        }

        fn show(what: &str, o: &Output) -> String {
            format!("{what}: {} stdout={:?} stderr={:?}", o.status, String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
        }

        #[test]
        fn container_confines_cmd() {
            let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("QH_SANDBOX_APPCONTAINER", "1"); // opt-in while it's a spike
            assert_eq!(unavailable(), None);
            let (root, a, b) = dirs("cmd");
            fs::write(b.join("secret.txt"), "TOPSECRET").unwrap();
            fs::create_dir_all(a.join(".git").join("hooks")).unwrap();
            fs::write(a.join(".git").join("hooks").join("keep"), "k").unwrap();
            let p = Policy { write: vec![a.clone()], protect: vec![a.join(".git").join("hooks")], ..Default::default() };

            let (o, _) = cmd(&p, "echo x> f.txt", &a, 30);
            assert!(o.status.success() && a.join("f.txt").is_file(), "{}", show("write A", &o));
            let (o, _) = cmd(&p, "type f.txt", &a, 30);
            assert!(o.status.success() && String::from_utf8_lossy(&o.stdout).contains('x'), "{}", show("read A", &o));
            let (o, _) = cmd(&p, r"echo x> ..\b\f.txt", &a, 30);
            assert!(!b.join("f.txt").exists(), "{}", show("write B", &o));
            let (o, _) = cmd(&p, r"type ..\b\secret.txt", &a, 30);
            assert!(!o.status.success() && !String::from_utf8_lossy(&o.stdout).contains("TOPSECRET"), "{}", show("read B", &o));
            // protect via deny ACEs didn't hold in the first CI run: reported while the AppContainer is a spike
            let (o, _) = cmd(&p, r"echo x> .git\hooks\pre-commit", &a, 30);
            eprintln!("SPIKE protect write blocked: {} ({})", !a.join(".git").join("hooks").join("pre-commit").exists(), show("write protect", &o));
            let (o, _) = cmd(&p, r"del /q .git\hooks\keep", &a, 30);
            eprintln!("SPIKE protect delete blocked: {} ({})", a.join(".git").join("hooks").join("keep").exists(), show("delete in protect", &o));
            let (o, _) = cmd(&p, "ren .git git2", &a, 30);
            eprintln!("SPIKE protect rename blocked: {} ({})", a.join(".git").join("hooks").is_dir(), show("rename protect's parent", &o));

            let (o, _) = cmd(&p, "curl.exe -s --max-time 5 https://example.com", &a, 30);
            assert!(!o.status.success(), "{}", show("curl Net::Off", &o));
            let open = Policy { net: Net::Open, ..p.clone() };
            let (o, _) = cmd(&open, "curl.exe -s -o NUL -w %{http_code} --max-time 10 https://example.com", &a, 30);
            eprintln!("SPIKE {}", show("curl Net::Open (expect 200)", &o));

            let t0 = Instant::now();
            let (_, killed) = cmd(&p, "for /l %i in (1,0,2) do @rem", &a, 2);
            assert!(killed && t0.elapsed() < Duration::from_secs(20), "timeout: killed={killed} after {:?}", t0.elapsed());

            let (o, killed) = cmd(&p, "for /l %i in (1,1,4000) do @echo 0123456789012345678901234567890123456789012345678", &a, 120);
            assert!(!killed && o.stdout.len() == 4000 * 51, "big output: {} bytes", o.stdout.len());

            let sid = container_sid().unwrap();
            assert!(reset().unwrap() >= 2); // A and the protect path at least
            for path in [&a, &a.join(".git").join("hooks")] {
                let (_sd, dacl) = read_dacl(path).unwrap();
                for ty in [ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE] {
                    assert!(!unsafe { has_ace(dacl, sid.0, ty, 0, 0) }, "ACE left on {}", path.display());
                }
            }
            let _ = fs::remove_dir_all(root);
        }

        /// The spike's key question: do Git Bash (MSYS2) and PowerShell work inside the container?
        /// Reports only (run with --nocapture to see it); never fails the suite.
        #[test]
        fn spike_shells() {
            let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
            let (root, a, _) = dirs("shells");
            let p = Policy { write: vec![a.clone()], ..Default::default() };
            let bash = ["ProgramFiles", "ProgramW6432"].iter().filter_map(std::env::var_os)
                .map(|b| PathBuf::from(b).join(r"Git\bin\bash.exe")).find(|b| b.is_file());
            match bash {
                Some(bash) => {
                    let args = ["-c".into(), "echo hi > f.txt && cat f.txt && ls / > /dev/null && git --version && echo BASH-OK".into()];
                    let r = run_timeout(&p, &bash, &args, &env_with_tmp(&a), 60, &a);
                    let host = Command::new(&bash).args(&args).current_dir(&a).output().map(|o| o.status.success());
                    eprintln!("SPIKE git-bash {}: {}", bash.display(), match r {
                        Ok((o, k)) => format!("works={} killed={k} {}", String::from_utf8_lossy(&o.stdout).contains("BASH-OK"), show("", &o)),
                        Err(e) => format!("spawn failed: {e}"),
                    });
                    eprintln!("SPIKE git-bash outside the container succeeds: {host:?}");
                }
                None => eprintln!("SPIKE git-bash: not installed"),
            }
            let ps = sys(r"WindowsPowerShell\v1.0\powershell.exe");
            let args = ["-NoProfile".into(), "-NonInteractive".into(), "-Command".into(),
                "Set-Content f2.txt hi; Get-Content f2.txt; 'PS-OK'".into()];
            eprintln!("SPIKE powershell: {}", match run_timeout(&p, &ps, &args, &env_with_tmp(&a), 60, &a) {
                Ok((o, k)) => format!("works={} killed={k} {}", String::from_utf8_lossy(&o.stdout).contains("PS-OK"), show("", &o)),
                Err(e) => format!("spawn failed: {e}"),
            });
            let _ = reset();
            let _ = fs::remove_dir_all(root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[u16]) -> String {
        String::from_utf16(v).unwrap()
    }

    #[test]
    fn sandbox_windows_quoting() {
        let args: Vec<OsString> = ["a", "b c", "", r#"a"b"#, r"a\b", r"a\\", r"a b\", r#"a\"b"#, "x\ty", r"\\srv\s p\"]
            .iter().map(OsString::from).collect();
        let line = command_line(OsStr::new(r"C:\Program Files\x.exe"), &args);
        assert_eq!(line.last(), Some(&0));
        assert_eq!(s(&line[..line.len() - 1]),
            r#""C:\Program Files\x.exe" a "b c" "" a\"b a\b a\\ "a b\\" a\\\"b "x	y" "\\srv\s p\\""#);
        let line = command_line(OsStr::new("cmd.exe"), &["/c".into(), r"echo x> A\f.txt".into()]);
        assert_eq!(s(&line[..line.len() - 1]), r#""cmd.exe" /c "echo x> A\f.txt""#);
    }

    #[test]
    fn sandbox_windows_env_block() {
        let e = |k: &str, v: &str| (OsString::from(k), OsString::from(v));
        let b = env_block(&[e("b", "2"), e("PATH", r"C:\x"), e("a", "1"), e("A", "3"), e("=C:", r"C:\w"), e("x=y", "z"), e("_U", "u")]);
        assert_eq!(s(&b), "=C:=C:\\w\0A=3\0b=2\0PATH=C:\\x\0_U=u\0\0");
        assert_eq!(env_block(&[]), vec![0, 0]);
    }

    #[test]
    fn sandbox_windows_paths() {
        assert_eq!(plain(Path::new(r"\\?\C:\x\y")), PathBuf::from(r"C:\x\y"));
        assert_eq!(plain(Path::new(r"\\?\UNC\srv\share\x")), PathBuf::from(r"\\srv\share\x"));
        assert_eq!(plain(Path::new(r"C:\x")), PathBuf::from(r"C:\x"));
        assert!(under(Path::new(r"\\?\C:\Users\K\proj\.env"), Path::new(r"c:\users\k\proj\")));
        assert!(under(Path::new(r"C:\proj"), Path::new(r"C:\proj")));
        assert!(!under(Path::new(r"C:\proj2"), Path::new(r"C:\proj")));
        assert!(under(Path::new(r"C:\x"), Path::new(r"C:\")));
    }

    #[test]
    fn sandbox_windows_grant_plan() {
        let t = std::env::temp_dir().join(format!("qh-sbxw-plan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let (home, proj) = (t.join("home"), t.join("proj"));
        for d in [home.join(".cargo"), home.join(".rustup"), home.join(".ssh"), home.join("AppData/Roaming/npm"),
                  home.join("AppData/Roaming/secretapp"), proj.join(".git/hooks")] {
            std::fs::create_dir_all(d).unwrap();
        }
        for f in [home.join(".cargo/credentials.toml"), proj.join(".git/config"), proj.join(".env")] {
            std::fs::write(f, "x").unwrap();
        }
        let p = Policy {
            write: vec![proj.clone(), t.join("missing")],
            deny_read: vec![home.join(".ssh"), home.join(".cargo/credentials.toml"), home.join(".rustup"), proj.join(".env")],
            protect: vec![proj.join(".git/hooks"), proj.join(".git/config"), proj.join(".qh"), home.join(".bashrc")],
            ..Default::default()
        };
        let got: Vec<(Kind, String, bool)> = plan(&p, &home).into_iter()
            .map(|(k, path, d)| (k, path.strip_prefix(&t).unwrap().to_string_lossy().replace('\\', "/"), d)).collect();
        let want: Vec<(Kind, String, bool)> = [
            (Kind::Write, "proj", true),
            (Kind::Read, "home/.cargo", true),
            (Kind::Read, "home/AppData/Roaming/npm", true),
            (Kind::DenyRead, "home/.cargo/credentials.toml", false),
            (Kind::DenyRead, "proj/.env", false),
            (Kind::DenyWrite, "proj/.git/hooks", true),
            (Kind::DenyDelete, "proj/.git", true),
            (Kind::DenyWrite, "proj/.git/config", false),
        ].iter().map(|(k, p, d)| (*k, p.to_string(), *d)).collect();
        assert_eq!(got, want);
        let _ = std::fs::remove_dir_all(&t);
    }
}
