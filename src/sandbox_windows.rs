//! Native-Windows sandbox for the model's shell commands (safety plan S4): two hidden local users.
//!
//! `qh sandbox setup` asks for admin once (UAC) and makes the group `qh-sandbox` with two members,
//! `qh-sbx-off` (no network) and `qh-sbx-on` (only qh's allowlist proxy on 127.0.0.1), as persistent
//! WFP filters keyed to each user. Their random passwords are kept with DPAPI for the current user.
//! A command then goes qh → `qh __winsbx` (a launcher, as you: it owns the job that kills the tree)
//! → `qh __winsbx-in` (CreateProcessWithLogonW, as the sandbox user) → the shell, under a
//! write-restricted token whose writes must also pass for the group: so it writes only where qh
//! granted the group (project, temp, package caches), not everywhere Users may. Your profile is
//! closed to other users; secrets inside a grant (or outside the profile) get deny ACEs, and so do
//! the protected paths. `qh sandbox remove` takes it all back.
//! The design is Codex's (codex-rs/windows-sandbox-rs, Apache-2.0): sandbox users, WFP filters,
//! write-restricted tokens, OWNER RIGHTS to keep the runner untouchable. No code is vendored.
//!
//! Only the pure helpers (quoting, environment block, choosing what to grant) build on other systems,
//! for their unit tests.
#![cfg_attr(not(windows), allow(dead_code))]
use crate::sandbox::Policy;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// Ports qh's proxy tries first when the sandbox users exist: the online user may reach only these.
pub const PROXY_PORTS: std::ops::RangeInclusive<u16> = 47610..=47625;
const GROUP: &str = "qh-sandbox";
const USERS: [&str; 2] = ["qh-sbx-off", "qh-sbx-on"];

/// What one ACE does for the sandbox group. Ordered so allows are applied before the denies that
/// carve exceptions out of them.
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
/// rustup, scoop shims, git's own config: git stops on a config file it can't read). Never the profile itself, .ssh, .aws, or AppData\Roaming as a whole.
const TOOLCHAIN: &[&str] = &[".cargo", ".rustup", ".gitconfig", ".config/git", "scoop", ".pyenv", "AppData/Local/Programs",
    "AppData/Roaming/npm", "AppData/Roaming/nvm", "AppData/Local/Volta"];

/// Every ACE a command needs as (kind, path, is_dir), allows first. Only existing paths; nothing in or
/// under `deny_read` is granted. A secret gets a deny of its own when something granted holds it or
/// it lies outside `profile` (your real profile is closed to the sandbox users already; elsewhere
/// Users may read).
fn plan(p: &Policy, home: &Path, profile: &Path) -> Vec<(Kind, PathBuf, bool)> {
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
    // cargo locks these on every registry access; the folder they're in stays read-only
    for f in [".package-cache", ".package-cache-mutate"] {
        let path = home.join(".cargo").join(f);
        if path.is_file() && !denied(&path) { v.push((Kind::Write, path, false)) }
    }
    let granted: Vec<PathBuf> = v.iter().map(|g| g.1.clone()).collect();
    for d in &p.deny_read {
        if let Ok(m) = d.metadata() {
            if granted.iter().any(|g| under(d, g)) || !under(d, profile) { v.push((Kind::DenyRead, d.clone(), m.is_dir())) }
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

/// A password for a sandbox user from 32 random bytes: every character class, so any policy takes it.
fn password(r: &[u8; 32]) -> String {
    const C: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789-_.+=@#%";
    format!("Qh7#{}", r.iter().map(|b| C[*b as usize % C.len()] as char).collect::<String>())
}

/// `GIT_CONFIG_*` entries that trust every repo: the project belongs to you, not the sandbox user,
/// and git refuses another user's repo ("dubious ownership"). Appended after any the caller set.
fn git_trust(count: Option<&str>) -> Vec<(String, String)> {
    let n: usize = count.and_then(|c| c.trim().parse().ok()).unwrap_or(0);
    vec![(format!("GIT_CONFIG_KEY_{n}"), "safe.directory".into()), (format!("GIT_CONFIG_VALUE_{n}"), "*".into()),
         ("GIT_CONFIG_COUNT".into(), (n + 1).to_string())]
}

#[cfg(not(windows))]
pub fn ready() -> bool {
    false
}

#[cfg(windows)]
pub use win::{admin_main, command, launcher_main, offer_once, ready, remove, reset, runner_main, setup, unavailable};

#[cfg(windows)]
mod win {
    use super::*;
    use crate::sandbox::Net;
    use std::ffi::c_void;
    use std::fs;
    use std::io::{self, Write};
    use std::mem::{size_of_val, zeroed};
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::ptr::{self, null, null_mut};
    use std::sync::{Mutex, OnceLock};
    use windows_sys::core::{GUID, PWSTR};
    use windows_sys::Win32::Foundation::{LocalFree, ERROR_ALIAS_EXISTS, ERROR_CANCELLED, ERROR_SUCCESS, HANDLE,
        INVALID_HANDLE_VALUE, FWP_E_ALREADY_EXISTS, FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND,
        FWP_E_PROVIDER_NOT_FOUND, FWP_E_SUBLAYER_NOT_FOUND};
    use windows_sys::Win32::NetworkManagement::NetManagement::{NetLocalGroupAdd, NetLocalGroupAddMembers,
        NetLocalGroupDel, NetUserAdd, NetUserDel, NetUserSetInfo, LOCALGROUP_INFO_1, LOCALGROUP_MEMBERS_INFO_3,
        NERR_GroupExists, NERR_UserExists, UF_DONT_EXPIRE_PASSWD, UF_PASSWD_CANT_CHANGE, UF_SCRIPT, USER_INFO_1,
        USER_INFO_1003, USER_PRIV_USER};
    use windows_sys::Win32::NetworkManagement::WindowsFilteringPlatform as wfp;
    use windows_sys::Win32::Security::Authorization::{ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        ConvertStringSidToSidW, GetNamedSecurityInfoW, SetEntriesInAclW, SetNamedSecurityInfoW, SetSecurityInfo, DENY_ACCESS,
        EXPLICIT_ACCESS_W, GRANT_ACCESS, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SE_KERNEL_OBJECT, TRUSTEE_IS_SID,
        TRUSTEE_IS_UNKNOWN, TRUSTEE_W};
    use windows_sys::Win32::Security::Cryptography::{CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN,
        CRYPT_INTEGER_BLOB};
    use windows_sys::Win32::Security::{AddAce, CreateRestrictedToken, EqualSid, GetAce, GetSecurityDescriptorDacl,
        GetSecurityDescriptorLength, GetTokenInformation, InitializeAcl, LookupAccountNameW, TokenGroups, TokenUser,
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, DISABLE_MAX_PRIVILEGE,
        INHERITED_ACE, INHERIT_ONLY_ACE, NO_INHERITANCE, OBJECT_INHERIT_ACE, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, PSID, SID_AND_ATTRIBUTES, TOKEN_ADJUST_DEFAULT, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
        TOKEN_GROUPS, TOKEN_QUERY, TOKEN_USER, WRITE_RESTRICTED};
    use windows_sys::Win32::Storage::FileSystem::{DELETE, FILE_APPEND_DATA, FILE_DELETE_CHILD, FILE_EXECUTE,
        FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_READ_DATA, FILE_READ_EA,
        FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FILE_WRITE_EA, WRITE_DAC, WRITE_OWNER};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE};
    use windows_sys::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_LOCAL_MACHINE, REG_DWORD};
    use windows_sys::Win32::System::Threading::{CreateProcessAsUserW, CreateProcessWithLogonW, GetCurrentProcess,
        GetCurrentThread, GetExitCodeProcess, OpenProcessToken, ResumeThread, TerminateProcess, WaitForSingleObject,
        CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE, LOGON_WITH_PROFILE, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOW};
    use windows_sys::Win32::UI::Shell::{DeleteProfileW, GetUserProfileDirectoryW, IsUserAnAdmin, ShellExecuteExW, SEE_MASK_NOASYNC,
        SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};

    // winnt.h; kept local so windows-sys needn't pull in Win32_System_SystemServices
    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;
    const SE_GROUP_LOGON_ID: u32 = 0xC000_0000;
    const RPC_C_AUTHN_DEFAULT: u32 = 0xFFFF_FFFF;

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

    fn win_err(what: &str, e: u32) -> io::Error {
        io::Error::other(format!("{what} failed: {}", io::Error::from_raw_os_error(e as i32)))
    }

    /// Memory the system LocalAlloc'd for us (security descriptors, ACLs, SIDs, strings).
    struct Local(*mut c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            if !self.0.is_null() { unsafe { LocalFree(self.0) }; }
        }
    }

    fn sid_from(s: &str) -> io::Result<Local> {
        let (w, mut sid) = (wz(s), null_mut());
        // SAFETY: NUL-terminated string; the SID is LocalFree'd by `Local`
        if unsafe { ConvertStringSidToSidW(w.as_ptr(), &mut sid) } == 0 { return Err(last_err()) }
        Ok(Local(sid))
    }

    fn sid_string(sid: PSID) -> io::Result<String> {
        let mut s: PWSTR = null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut s) } == 0 { return Err(last_err()) }
        let _f = Local(s.cast());
        let n = (0..).take_while(|&i| unsafe { *s.add(i) } != 0).count();
        Ok(String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(s, n) }))
    }

    /// A local account's or group's SID as a string.
    fn lookup(name: &str) -> io::Result<String> {
        let (w, mut sid, mut dom) = (wz(name), vec![0u64; 16], vec![0u16; 256]);
        let (mut cb, mut cd, mut use_) = ((sid.len() * 8) as u32, dom.len() as u32, 0);
        if unsafe { LookupAccountNameW(null(), w.as_ptr(), sid.as_mut_ptr().cast(), &mut cb, dom.as_mut_ptr(), &mut cd, &mut use_) } == 0 {
            return Err(io::Error::other(format!("{name}: {}", last_err())));
        }
        sid_string(sid.as_mut_ptr().cast())
    }

    /// Your real profile folder, from the process token (USERPROFILE can be pointed elsewhere).
    fn profile() -> &'static Path {
        static P: OnceLock<PathBuf> = OnceLock::new();
        P.get_or_init(|| {
            let mut tok = null_mut();
            let mut buf = vec![0u16; 1024];
            let mut n = buf.len() as u32;
            // SAFETY: the token handle is closed by OwnedHandle; buf holds n u16s
            let ok = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) } != 0 && {
                let _t = unsafe { OwnedHandle::from_raw_handle(tok) };
                (unsafe { GetUserProfileDirectoryW(tok, buf.as_mut_ptr(), &mut n) }) != 0
            };
            if !ok { return crate::platform::home() }
            PathBuf::from(String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]))
        })
    }

    /// qh's sandbox state in the real profile, whatever LOCALAPPDATA says.
    fn dir() -> PathBuf {
        profile().join(r"AppData\Local\qh")
    }

    fn secrets_file() -> PathBuf {
        dir().join("sandbox-users.dat")
    }

    pub fn ready() -> bool {
        secrets_file().is_file()
    }

    /// (group SID, offline password, online password), from the DPAPI blob.
    struct Creds { group: String, pw: [String; 2] }

    fn dpapi(data: &[u8], protect: bool) -> io::Result<Vec<u8>> {
        let inb = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: null_mut() };
        // SAFETY: `inb` points at `data`; `out` is LocalAlloc'd by the call and freed below
        let ok = unsafe {
            if protect { CryptProtectData(&inb, null(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) }
            else { CryptUnprotectData(&inb, null_mut(), null(), null(), null(), CRYPTPROTECT_UI_FORBIDDEN, &mut out) }
        };
        if ok == 0 { return Err(last_err()) }
        let _f = Local(out.pbData.cast());
        Ok(unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec())
    }

    fn load() -> io::Result<Creds> {
        let raw = dpapi(&fs::read(secrets_file())?, false)?;
        let t = String::from_utf8_lossy(&raw).to_string();
        let l: Vec<&str> = t.lines().collect();
        if l.len() < 3 { return Err(io::Error::other("sandbox users: the stored secrets are damaged; run `qh sandbox setup` again")) }
        Ok(Creds { group: l[0].into(), pw: [l[1].into(), l[2].into()] })
    }

    /// Why commands can't be sandboxed here, or None once `qh sandbox setup` has run.
    pub fn unavailable() -> Option<String> {
        (!ready()).then(|| "not set up on this PC: commands run unsandboxed (the secret, write and environment guards \
still apply). Run `qh sandbox setup` (asks for admin once) to run them as a locked-down sandbox user.".into())
    }

    /// Once, the first time qh starts on a PC without the sandbox users: how to set them up.
    pub fn offer_once() -> Option<String> {
        let mark = dir().join("sandbox-offered");
        if ready() || mark.exists() { return None }
        let _ = fs::create_dir_all(dir()).and_then(|_| fs::write(&mark, ""));
        Some("Commands run unsandboxed on this PC. `/sandbox setup` (or `qh sandbox setup`) sandboxes them: \
Windows asks for admin once to add two hidden sandbox users. Until then qh asks before commands.".into())
    }

    // ---- ACLs: grants for the group, recorded so `remove` can take them back ----

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
    unsafe fn has_ace(dacl: *const ACL, sid: PSID, ty: u8, mask: u32, flags: u32) -> bool {
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
        dir().join("sandbox-grants.txt")
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
        let mut v = plan(p, &home, profile());
        // the runner is qh itself, started as the sandbox user: it must be able to run this file
        if let Ok(exe) = std::env::current_exe() { v.push((Kind::Read, exe, false)) }
        let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
        for (k, path, dir) in v {
            let key = (k, norm(&path));
            if done.contains(&key) { continue }
            match add_ace(&path, sid, k, dir) {
                Ok(()) => { done.insert(key); }
                Err(_) if k == Kind::Read && dir => {} // best effort: that tool just won't run inside
                // a deny that didn't land must not run the command; a failed write grant would only confuse
                Err(e) => return Err(io::Error::new(e.kind(), format!("sandbox: can't set access on {}: {e}", path.display()))),
            }
        }
        Ok(())
    }

    /// The command that runs `program args…` as a sandbox user: `qh __winsbx on|off program args…`,
    /// after the grants `p` needs are in place. The caller adds the environment and runs it.
    pub fn command(p: &Policy, program: &Path, args: &[&str]) -> io::Result<std::process::Command> {
        let c = load()?;
        let sid = sid_from(&c.group)?;
        grant(p, sid.0)?;
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.arg("__winsbx").arg(if p.net == Net::Off { "off" } else { "on" }).arg(program).args(args);
        cmd.envs(git_trust(std::env::var("GIT_CONFIG_COUNT").ok().as_deref()));
        Ok(cmd)
    }

    fn std_handles(si: &mut STARTUPINFOW) {
        si.cb = size_of::<STARTUPINFOW>() as u32;
        si.dwFlags = STARTF_USESTDHANDLES;
        // SAFETY: plain getters; std made these inheritable when it started us
        unsafe { (si.hStdInput, si.hStdOutput, si.hStdError) = (GetStdHandle(STD_INPUT_HANDLE), GetStdHandle(STD_OUTPUT_HANDLE), GetStdHandle(STD_ERROR_HANDLE)) };
    }

    fn fail(what: &str, e: impl std::fmt::Display) -> i32 {
        eprintln!("qh sandbox: {what}: {e}");
        126
    }

    /// `qh __winsbx on|off program args…`, as you: start the runner as that sandbox user in a job
    /// that dies with this process (so a timeout or task_stop takes the whole tree), wait, pass on
    /// its exit code.
    pub fn launcher_main(a: &[OsString]) -> i32 {
        let (Some(which), Some(prog)) = (a.first(), a.get(1)) else { return fail("usage", "qh __winsbx on|off program args…") };
        let c = match load() { Ok(c) => c, Err(e) => return fail("can't read the sandbox users' secrets", e) };
        let i = usize::from(which == "on");
        let Ok(exe) = std::env::current_exe() else { return fail("can't find qh", last_err()) };
        let mut args: Vec<OsString> = vec!["__winsbx-in".into(), c.group.clone().into(), which.clone(), prog.clone()];
        args.extend(a[2..].iter().cloned());
        let mut cmdline = command_line(exe.as_os_str(), &args);
        let env: Vec<_> = std::env::vars_os().collect();
        let (envb, cwd, app) = (env_block(&env), wz(plain(&std::env::current_dir().unwrap_or_default())), wz(&exe));
        let (user, domain, pw) = (wz(USERS[i]), wz("."), wz(&c.pw[i]));
        let mut si: STARTUPINFOW = unsafe { zeroed() };
        std_handles(&mut si);
        let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
        // SAFETY: all pointers are to live NUL-terminated buffers. The profile is loaded: without it
        // Windows' TLS (schannel) has no credentials, so every HTTPS download fails; `remove` deletes it
        let ok = unsafe { CreateProcessWithLogonW(user.as_ptr(), domain.as_ptr(), pw.as_ptr(), LOGON_WITH_PROFILE, app.as_ptr(), cmdline.as_mut_ptr(),
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW, envb.as_ptr().cast(), cwd.as_ptr(), &si, &mut pi) };
        if ok == 0 {
            let e = last_err();
            let fix = match e.raw_os_error() {
                Some(1326 | 1327 | 1331) => " (the sandbox user's login was refused: run `qh sandbox setup` again)",
                Some(1058) => " (the Secondary Logon service is disabled: set it to Manual in services.msc)",
                _ => "",
            };
            return fail(&format!("can't start the command as {}{fix}", USERS[i]), e);
        }
        let (process, thread) = unsafe { (OwnedHandle::from_raw_handle(pi.hProcess), OwnedHandle::from_raw_handle(pi.hThread)) };
        // locked while still suspended: a command already running as this user can't slip in before
        // the runner locks itself (our handles were opened before, so we keep full access)
        if let Err(e) = lock([process.as_raw_handle(), thread.as_raw_handle()]) {
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            return fail("can't protect the runner", e);
        }
        // the job exists before the runner resumes, so nothing it starts escapes; best effort, since
        // the logon service may have put it in a job that can't nest (old Windows)
        let job = unsafe { CreateJobObjectW(null(), null()) };
        let _job = (!job.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(job) });
        if !job.is_null() {
            let mut lim: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
            lim.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            unsafe {
                SetInformationJobObject(job, JobObjectExtendedLimitInformation, ptr::from_ref(&lim).cast(), size_of_val(&lim) as u32);
                AssignProcessToJobObject(job, process.as_raw_handle());
            }
        }
        if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
            unsafe { TerminateProcess(process.as_raw_handle(), 1) };
            return fail("can't resume the runner", last_err());
        }
        let mut code = 1u32;
        unsafe {
            WaitForSingleObject(process.as_raw_handle(), INFINITE);
            GetExitCodeProcess(process.as_raw_handle(), &mut code);
        }
        code as i32
    }

    /// `qh __winsbx-in <group sid> on|off program args…`, as the sandbox user: run the command under a
    /// copy of this token without privileges (offline: also write-restricted), wait, pass on its exit code.
    pub fn runner_main(a: &[OsString]) -> i32 {
        let (Some(group), Some(which), Some(prog)) = (a.first(), a.get(1), a.get(2)) else { return fail("usage", "qh __winsbx-in sid on|off program args…") };
        // the command runs as the same user: without this it could open this (unrestricted) process
        // and write through it. OWNER RIGHTS limits the owner to reading the DACL, so it can't be undone.
        if let Err(e) = lock_self() { return fail("can't protect the runner", e) }
        let token = match restricted(&group.to_string_lossy(), which != "on") { Ok(t) => t, Err(e) => return fail("can't make the restricted token", e) };
        let mut cmdline = command_line(prog, &a[3..]);
        let p = PathBuf::from(prog);
        let app = p.is_absolute().then(|| wz(&p)); // else CreateProcess looks on PATH
        let env: Vec<_> = std::env::vars_os().collect();
        let envb = env_block(&env);
        let mut si: STARTUPINFOW = unsafe { zeroed() };
        std_handles(&mut si);
        let mut pi: PROCESS_INFORMATION = unsafe { zeroed() };
        // SAFETY: live NUL-terminated buffers; the token is ours; inherits only std handles (all qh has here)
        let ok = unsafe { CreateProcessAsUserW(token.as_raw_handle(), app.as_ref().map_or(null(), |a| a.as_ptr()), cmdline.as_mut_ptr(),
            null(), null(), 1, CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW, envb.as_ptr().cast(), null(), &si, &mut pi) };
        if ok == 0 { return fail(&format!("can't start {}", p.display()), last_err()) }
        let (process, _thread) = unsafe { (OwnedHandle::from_raw_handle(pi.hProcess), OwnedHandle::from_raw_handle(pi.hThread)) };
        let mut code = 1u32;
        unsafe {
            WaitForSingleObject(process.as_raw_handle(), INFINITE);
            GetExitCodeProcess(process.as_raw_handle(), &mut code);
        }
        code as i32
    }

    fn lock_self() -> io::Result<()> {
        lock(unsafe { [GetCurrentProcess(), GetCurrentThread()] })
    }

    /// SYSTEM gets everything, the owner (the sandbox user) only reads the DACL.
    fn lock(handles: [HANDLE; 2]) -> io::Result<()> {
        let w = wz("D:P(A;;GA;;;SY)(A;;RC;;;OW)");
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), 1, &mut sd, null_mut()) } == 0 { return Err(last_err()) }
        let _sd = Local(sd);
        let (mut present, mut dacl, mut def) = (0, null_mut(), 0);
        if unsafe { GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut def) } == 0 { return Err(last_err()) }
        for h in handles {
            let e = unsafe { SetSecurityInfo(h, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                null_mut(), null_mut(), dacl, null()) };
            if e != ERROR_SUCCESS { return Err(win_err("SetSecurityInfo", e)) }
        }
        Ok(())
    }

    /// TokenUser / TokenGroups into a u64-aligned buffer.
    fn token_info(tok: HANDLE, class: i32) -> io::Result<Vec<u64>> {
        let mut n = 0;
        unsafe { GetTokenInformation(tok, class, null_mut(), 0, &mut n) };
        let mut buf = vec![0u64; (n as usize).div_ceil(8).max(1)];
        if unsafe { GetTokenInformation(tok, class, buf.as_mut_ptr().cast(), n, &mut n) } == 0 { return Err(last_err()) }
        Ok(buf)
    }

    /// This process's token with no privileges but traverse; with `write`, also write-restricted to
    /// the sandbox group, the user, the logon session and Everyone (the NUL device and other such
    /// objects). Online commands go without the write restriction: Windows' TLS (Schannel) refuses
    /// every write-restricted token (SEC_E_NO_CREDENTIALS; Codex has the same bug), so cargo,
    /// PowerShell and dotnet couldn't download. The shared spots any user may write are closed to
    /// the group at setup instead (`public_denies`).
    fn restricted(group: &str, write: bool) -> io::Result<OwnedHandle> {
        let mut tok = null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_DUPLICATE | TOKEN_QUERY | TOKEN_ASSIGN_PRIMARY | TOKEN_ADJUST_DEFAULT, &mut tok) } == 0 {
            return Err(last_err());
        }
        let tok = unsafe { OwnedHandle::from_raw_handle(tok) };
        let (user, groups) = (token_info(tok.as_raw_handle(), TokenUser)?, token_info(tok.as_raw_handle(), TokenGroups)?);
        let (g, everyone) = (sid_from(group)?, sid_from("S-1-1-0")?);
        let mut sids = vec![g.0, everyone.0, unsafe { (*(user.as_ptr() as *const TOKEN_USER)).User.Sid }];
        // SAFETY: TOKEN_GROUPS with GroupCount entries, inside `groups`
        unsafe {
            let tg = groups.as_ptr() as *const TOKEN_GROUPS;
            let list = std::slice::from_raw_parts((*tg).Groups.as_ptr(), (*tg).GroupCount as usize);
            sids.extend(list.iter().filter(|x| x.Attributes & SE_GROUP_LOGON_ID == SE_GROUP_LOGON_ID).map(|x| x.Sid));
        }
        let mut sa: Vec<SID_AND_ATTRIBUTES> = sids.iter().map(|&s| SID_AND_ATTRIBUTES { Sid: s, Attributes: 0 }).collect();
        let mut out = null_mut();
        let (flags, n) = if write { (DISABLE_MAX_PRIVILEGE | WRITE_RESTRICTED, sa.len() as u32) } else { (DISABLE_MAX_PRIVILEGE, 0) };
        if unsafe { CreateRestrictedToken(tok.as_raw_handle(), flags, 0, null(), 0, null(), n,
            if n == 0 { null_mut() } else { sa.as_mut_ptr() }, &mut out) } == 0 { return Err(last_err()) }
        Ok(unsafe { OwnedHandle::from_raw_handle(out) })
    }

    // ---- setup and remove ----

    fn is_admin() -> bool {
        unsafe { IsUserAnAdmin() != 0 }
    }

    /// Run `qh __winsbx-admin <args>` elevated (the UAC prompt) and wait; its exit code.
    fn elevated(args: &str) -> io::Result<i32> {
        let (verb, exe, params) = (wz("runas"), wz(std::env::current_exe()?), wz(format!("__winsbx-admin {args}")));
        let mut sei: SHELLEXECUTEINFOW = unsafe { zeroed() };
        sei.cbSize = size_of::<SHELLEXECUTEINFOW>() as u32;
        sei.fMask = SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC;
        (sei.lpVerb, sei.lpFile, sei.lpParameters, sei.nShow) = (verb.as_ptr(), exe.as_ptr(), params.as_ptr(), 0);
        if unsafe { ShellExecuteExW(&mut sei) } == 0 {
            let e = last_err();
            return Err(if e.raw_os_error() == Some(ERROR_CANCELLED as i32) { io::Error::other("the admin prompt was declined") } else { e });
        }
        if sei.hProcess.is_null() || sei.hProcess == INVALID_HANDLE_VALUE { return Err(io::Error::other("no admin process to wait for")) }
        let h = unsafe { OwnedHandle::from_raw_handle(sei.hProcess) };
        let mut code = 1u32;
        unsafe {
            WaitForSingleObject(h.as_raw_handle(), INFINITE);
            GetExitCodeProcess(h.as_raw_handle(), &mut code);
        }
        Ok(code as i32)
    }

    /// The admin step for `setup`/`remove`, here if we already are admin, else through UAC. Its errors
    /// come back in `<dir>\sandbox-admin.log` (an elevated window closes before anyone reads it).
    fn as_admin(what: &str) -> Result<(), String> {
        let log = dir().join("sandbox-admin.log");
        let _ = fs::remove_file(&log);
        let arg = format!("{what} \"{}\"", dir().display());
        let code = if is_admin() { admin_main(&[what.into(), dir().into_os_string()]) } else { elevated(&arg).map_err(|e| e.to_string())? };
        let msg = fs::read_to_string(&log).unwrap_or_default();
        let _ = fs::remove_file(&log);
        if code == 0 { Ok(()) } else { Err(if msg.trim().is_empty() { format!("the admin step failed (exit {code})") } else { msg.trim().into() }) }
    }

    /// `qh sandbox setup`: make (or repair) the sandbox users. Safe to run again: passwords are renewed.
    pub fn setup() -> Result<String, String> {
        let d = dir();
        fs::create_dir_all(&d).map_err(|e| e.to_string())?;
        let pw = [password(&crate::oauth::random()), password(&crate::oauth::random())];
        // the passwords go to the admin step in a file in your own profile (other users can't read
        // it), which it deletes; the command line would show them to every process
        let hand = d.join("sandbox-setup.tmp");
        fs::write(&hand, format!("{}\n{}\n", pw[0], pw[1])).map_err(|e| e.to_string())?;
        let r = as_admin("setup");
        let _ = fs::remove_file(&hand);
        r?;
        let group = lookup(GROUP).map_err(|e| e.to_string())?;
        let blob = dpapi(format!("{group}\n{}\n{}\n", pw[0], pw[1]).as_bytes(), true).map_err(|e| e.to_string())?;
        fs::write(secrets_file(), blob).map_err(|e| e.to_string())?;
        DONE.lock().unwrap_or_else(|e| e.into_inner()).clear();
        Ok(format!("sandbox users ready: commands now run as {} (no network) or {} (only qh's allowlist proxy), \
writing only in the project, temp and package caches. Restart qh for auto mode. `qh sandbox remove` undoes it.", USERS[0], USERS[1]))
    }

    /// `qh sandbox remove`: the ACEs, the users, their profiles and group, the firewall filters, the stored secrets.
    pub fn remove() -> Result<String, String> {
        let n = reset().map_err(|e| format!("taking back folder permissions: {e}"))?;
        as_admin("remove")?;
        for f in [secrets_file(), dir().join("sandbox-offered")] {
            let _ = fs::remove_file(f);
        }
        Ok(format!("removed the sandbox users, their firewall filters and stored passwords, and their access to {n} folders"))
    }

    /// Remove every ACE qh gave the sandbox group (`qh sandbox reset`); how many paths were cleaned.
    /// Paths that fail stay listed so a later reset can retry them.
    pub fn reset() -> io::Result<usize> {
        let f = grants_file();
        let text = match fs::read_to_string(&f) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let group = load().map(|c| c.group).or_else(|_| lookup(GROUP))?;
        let sid = sid_from(&group)?;
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

    /// `qh __winsbx-admin setup|remove <state dir>`, elevated. Errors go to `<dir>\sandbox-admin.log`.
    pub fn admin_main(a: &[OsString]) -> i32 {
        let (Some(what), Some(d)) = (a.first().and_then(|x| x.to_str()), a.get(1).map(PathBuf::from)) else { return 2 };
        let r = match what {
            "setup" => admin_setup(&d),
            "remove" => admin_remove(),
            _ => Err(io::Error::other("usage: qh __winsbx-admin setup|remove <dir>")),
        };
        match r {
            Ok(()) => 0,
            Err(e) => { let _ = fs::write(d.join("sandbox-admin.log"), e.to_string()); 1 }
        }
    }

    const HIDE: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Winlogon\SpecialAccounts\UserList";

    fn admin_setup(d: &Path) -> io::Result<()> {
        let hand = d.join("sandbox-setup.tmp");
        let t = fs::read_to_string(&hand)?;
        let _ = fs::remove_file(&hand);
        let pw: Vec<&str> = t.lines().collect();
        if pw.len() < 2 { return Err(io::Error::other("no passwords handed over")) }
        let (g, comment) = (wz(GROUP), wz("qh: sandbox users for the model's commands"));
        let info = LOCALGROUP_INFO_1 { lgrpi1_name: g.as_ptr() as PWSTR, lgrpi1_comment: comment.as_ptr() as PWSTR };
        let e = unsafe { NetLocalGroupAdd(null(), 1, ptr::from_ref(&info).cast(), null_mut()) };
        if e != 0 && e != NERR_GroupExists && e != ERROR_ALIAS_EXISTS { return Err(win_err("NetLocalGroupAdd", e)) }
        let mut sids = vec![];
        for (u, p) in USERS.iter().zip(&pw) {
            let (name, pass) = (wz(u), wz(p));
            let info = USER_INFO_1 { usri1_name: name.as_ptr() as PWSTR, usri1_password: pass.as_ptr() as PWSTR, usri1_password_age: 0,
                usri1_priv: USER_PRIV_USER, usri1_home_dir: null_mut(), usri1_comment: comment.as_ptr() as PWSTR,
                usri1_flags: UF_SCRIPT | UF_DONT_EXPIRE_PASSWD | UF_PASSWD_CANT_CHANGE, usri1_script_path: null_mut() };
            let e = unsafe { NetUserAdd(null(), 1, ptr::from_ref(&info).cast(), null_mut()) };
            if e == NERR_UserExists {
                let info = USER_INFO_1003 { usri1003_password: pass.as_ptr() as PWSTR };
                let e = unsafe { NetUserSetInfo(null(), name.as_ptr(), 1003, ptr::from_ref(&info).cast(), null_mut()) };
                if e != 0 { return Err(win_err(&format!("resetting {u}'s password"), e)) }
            } else if e != 0 {
                return Err(win_err(&format!("NetUserAdd {u}"), e));
            }
            let m = LOCALGROUP_MEMBERS_INFO_3 { lgrmi3_domainandname: name.as_ptr() as PWSTR };
            unsafe { NetLocalGroupAddMembers(null(), g.as_ptr(), 3, ptr::from_ref(&m).cast(), 1) }; // already a member is fine
            // off the sign-in screen
            let (k, zero) = (wz(HIDE), 0u32);
            unsafe { RegSetKeyValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), name.as_ptr(), REG_DWORD, ptr::from_ref(&zero).cast(), 4) };
            sids.push(lookup(u)?);
        }
        firewall(&sids[0], &sids[1])?;
        let g = lookup(GROUP)?;
        let sid = sid_from(&g)?;
        for (path, inherit) in public_spots() {
            let (mask, flags) = if inherit { (NO_WRITE, OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE) } else { (FILE_WRITE_DATA | FILE_APPEND_DATA, NO_INHERITANCE) };
            deny(&path, sid.0, mask, flags)?;
        }
        Ok(())
    }

    /// Places every user may write, closed to the sandbox group (online commands aren't
    /// write-restricted): no new files or folders at the top of each fixed drive or in ProgramData,
    /// nothing at all under Users\Public. (Folders some app opened to all users stay open: a known gap.)
    fn public_spots() -> Vec<(PathBuf, bool)> {
        use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
        const DRIVE_FIXED: u32 = 3;
        let mut v = vec![];
        let mask = unsafe { GetLogicalDrives() };
        for i in 0..26u8 {
            let root = format!("{}:\\", (b'A' + i) as char);
            if mask & (1 << i) != 0 && unsafe { GetDriveTypeW(wz(&root).as_ptr()) } == DRIVE_FIXED { v.push((PathBuf::from(root), false)) }
        }
        let env = |k: &str, d: &str| std::env::var_os(k).map(PathBuf::from).unwrap_or_else(|| PathBuf::from(d));
        v.push((env("ProgramData", r"C:\ProgramData"), false));
        v.push((env("PUBLIC", r"C:\Users\Public"), true));
        v.into_iter().filter(|(p, _)| p.is_dir()).collect()
    }

    /// One deny ACE for `sid` on `path` (admin only: these folders aren't ours).
    fn deny(path: &Path, sid: PSID, mask: u32, flags: u32) -> io::Result<()> {
        let (_sd, dacl) = read_dacl(path)?;
        if unsafe { has_ace(dacl, sid, ACCESS_DENIED_ACE_TYPE, mask, flags) } { return Ok(()) }
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: mask, grfAccessMode: DENY_ACCESS, grfInheritance: flags,
            Trustee: TRUSTEE_W { pMultipleTrustee: null_mut(), MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID, TrusteeType: TRUSTEE_IS_UNKNOWN, ptstrName: sid as PWSTR },
        };
        let mut new: *mut ACL = null_mut();
        let e = unsafe { SetEntriesInAclW(1, &ea, dacl, &mut new) };
        if e != ERROR_SUCCESS { return Err(win_err(&format!("denying {}", path.display()), e)) }
        let _new = Local(new.cast());
        write_dacl(path, new).map_err(|e| io::Error::other(format!("denying {}: {e}", path.display())))
    }

    fn admin_remove() -> io::Result<()> {
        let fw = firewall_remove();
        // the public denies, while the group's SID still resolves
        if let Ok(sid) = lookup(GROUP).and_then(|g| sid_from(&g)) {
            for (path, _) in public_spots() { let _ = strip(&path, sid.0); }
        }
        for u in USERS {
            let name = wz(u);
            // its profile (C:\Users\qh-sbx-…), made by the first command; needs the SID, so before the user goes
            if let Ok(sid) = lookup(u) { unsafe { DeleteProfileW(wz(&sid).as_ptr(), null(), null()) }; }
            unsafe {
                NetUserDel(null(), name.as_ptr());
                RegDeleteKeyValueW(HKEY_LOCAL_MACHINE, wz(HIDE).as_ptr(), name.as_ptr());
            }
        }
        unsafe { NetLocalGroupDel(null(), wz(GROUP).as_ptr()) };
        fw
    }

    // ---- WFP: per-user firewall filters, persistent across reboots ----

    const PROVIDER: GUID = GUID::from_u128(0x6a1f_3c52_8e0d_4b7a_9f21_57c3_0d4e_a100);
    const SUBLAYER: GUID = GUID::from_u128(0x6a1f_3c52_8e0d_4b7a_9f21_57c3_0d4e_a101);
    /// filter keys are FILTER_BASE + 0..N_FILTERS
    const FILTER_BASE: u128 = 0x6a1f_3c52_8e0d_4b7a_9f21_57c3_0d4e_b000;
    const N_FILTERS: u128 = 32;

    struct Engine(HANDLE);
    impl Drop for Engine {
        fn drop(&mut self) {
            unsafe { wfp::FwpmEngineClose0(self.0) };
        }
    }

    fn fwp(what: &str, e: u32, ok: &[i32]) -> io::Result<()> {
        if e == 0 || ok.contains(&(e as i32)) { Ok(()) } else { Err(io::Error::other(format!("{what} failed: 0x{e:08X}"))) }
    }

    /// Open the engine and run `f` in one transaction: all filters or none.
    fn in_txn(f: impl FnOnce(HANDLE) -> io::Result<()>) -> io::Result<()> {
        let mut h = null_mut();
        fwp("FwpmEngineOpen0", unsafe { wfp::FwpmEngineOpen0(null(), RPC_C_AUTHN_DEFAULT, null(), null(), &mut h) }, &[])?;
        let e = Engine(h);
        fwp("FwpmTransactionBegin0", unsafe { wfp::FwpmTransactionBegin0(e.0, 0) }, &[])?;
        match f(e.0) {
            Ok(()) => fwp("FwpmTransactionCommit0", unsafe { wfp::FwpmTransactionCommit0(e.0) }, &[]),
            Err(err) => { unsafe { wfp::FwpmTransactionAbort0(e.0) }; Err(err) }
        }
    }

    fn display(name: &[u16]) -> wfp::FWPM_DISPLAY_DATA0 {
        wfp::FWPM_DISPLAY_DATA0 { name: name.as_ptr() as PWSTR, description: null_mut() }
    }

    fn val8(v: u8) -> wfp::FWP_VALUE0 {
        wfp::FWP_VALUE0 { r#type: wfp::FWP_UINT8, Anonymous: wfp::FWP_VALUE0_0 { uint8: v } }
    }

    fn val16(v: u16) -> wfp::FWP_VALUE0 {
        wfp::FWP_VALUE0 { r#type: wfp::FWP_UINT16, Anonymous: wfp::FWP_VALUE0_0 { uint16: v } }
    }

    /// The offline user: no connection out or in. The online user: out only to loopback on
    /// PROXY_PORTS. Both may accept connections from loopback (a dev server your browser opens).
    /// DNS lookups go through the DNS Client service, not the user, so names still resolve.
    fn firewall(off: &str, on: &str) -> io::Result<()> {
        use wfp::*;
        let (connect, accept) = ([FWPM_LAYER_ALE_AUTH_CONNECT_V4, FWPM_LAYER_ALE_AUTH_CONNECT_V6],
                                 [FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V4, FWPM_LAYER_ALE_AUTH_RECV_ACCEPT_V6]);
        let name = wz("qh sandbox");
        in_txn(|h| {
            let mut provider: FWPM_PROVIDER0 = unsafe { zeroed() };
            (provider.providerKey, provider.displayData, provider.flags) = (PROVIDER, display(&name), FWPM_PROVIDER_FLAG_PERSISTENT);
            fwp("FwpmProviderAdd0", unsafe { FwpmProviderAdd0(h, &provider, null_mut()) }, &[FWP_E_ALREADY_EXISTS])?;
            let mut pk = PROVIDER;
            let mut sub: FWPM_SUBLAYER0 = unsafe { zeroed() };
            (sub.subLayerKey, sub.displayData, sub.flags, sub.providerKey, sub.weight) =
                (SUBLAYER, display(&name), FWPM_SUBLAYER_FLAG_PERSISTENT, &mut pk, 0x8000);
            fwp("FwpmSubLayerAdd0", unsafe { FwpmSubLayerAdd0(h, &sub, null_mut()) }, &[FWP_E_ALREADY_EXISTS])?;
            delete_filters(h)?;
            let mut n = 0u128;
            for (u, sid) in [off, on].iter().enumerate() {
                // matches connections made by this account: an SD granting it FWP_ACTRL_MATCH_FILTER
                let w = wz(format!("O:LSD:(A;;CC;;;{sid})"));
                let (mut sd, mut len) = (null_mut(), 0);
                if unsafe { ConvertStringSecurityDescriptorToSecurityDescriptorW(w.as_ptr(), 1, &mut sd, &mut len) } == 0 { return Err(last_err()) }
                let _sd = Local(sd);
                let mut blob = FWP_BYTE_BLOB { size: unsafe { GetSecurityDescriptorLength(sd) }, data: sd.cast() };
                let mut range = FWP_RANGE0 { valueLow: val16(*PROXY_PORTS.start()), valueHigh: val16(*PROXY_PORTS.end()) };
                let user = FWPM_FILTER_CONDITION0 { fieldKey: FWPM_CONDITION_ALE_USER_ID, matchType: FWP_MATCH_EQUAL,
                    conditionValue: FWP_CONDITION_VALUE0 { r#type: FWP_SECURITY_DESCRIPTOR_TYPE, Anonymous: FWP_CONDITION_VALUE0_0 { sd: &mut blob } } };
                let lo = FWPM_FILTER_CONDITION0 { fieldKey: FWPM_CONDITION_FLAGS, matchType: FWP_MATCH_FLAGS_ALL_SET,
                    conditionValue: FWP_CONDITION_VALUE0 { r#type: FWP_UINT32, Anonymous: FWP_CONDITION_VALUE0_0 { uint32: FWP_CONDITION_FLAG_IS_LOOPBACK } } };
                let port = FWPM_FILTER_CONDITION0 { fieldKey: FWPM_CONDITION_IP_REMOTE_PORT, matchType: FWP_MATCH_RANGE,
                    conditionValue: FWP_CONDITION_VALUE0 { r#type: FWP_RANGE_TYPE, Anonymous: FWP_CONDITION_VALUE0_0 { rangeValue: &mut range } } };
                // (layer, permit, conditions): in one sublayer the heavier matching filter decides
                let mut rules: Vec<(GUID, bool, Vec<FWPM_FILTER_CONDITION0>)> = vec![];
                for l in connect.iter().chain(&accept) { rules.push((*l, false, vec![user])) }
                for l in accept { rules.push((l, true, vec![user, lo])) }
                if u == 1 {
                    for l in connect { rules.push((l, true, vec![user, lo, port])) }
                }
                for (layer, permit, mut conds) in rules {
                    let mut f: FWPM_FILTER0 = unsafe { zeroed() };
                    f.filterKey = GUID::from_u128(FILTER_BASE + n);
                    n += 1;
                    (f.displayData, f.flags, f.providerKey, f.layerKey, f.subLayerKey) =
                        (display(&name), FWPM_FILTER_FLAG_PERSISTENT, &mut pk, layer, SUBLAYER);
                    f.weight = val8(if permit { 10 } else { 1 });
                    (f.numFilterConditions, f.filterCondition) = (conds.len() as u32, conds.as_mut_ptr());
                    f.action.r#type = if permit { FWP_ACTION_PERMIT } else { FWP_ACTION_BLOCK };
                    let mut id = 0;
                    fwp("FwpmFilterAdd0", unsafe { FwpmFilterAdd0(h, &f, null_mut(), &mut id) }, &[])?;
                }
            }
            debug_assert!(n <= N_FILTERS);
            Ok(())
        })
    }

    fn delete_filters(h: HANDLE) -> io::Result<()> {
        for i in 0..N_FILTERS {
            let k = GUID::from_u128(FILTER_BASE + i);
            fwp("FwpmFilterDeleteByKey0", unsafe { wfp::FwpmFilterDeleteByKey0(h, &k) }, &[FWP_E_FILTER_NOT_FOUND, FWP_E_NOT_FOUND])?;
        }
        Ok(())
    }

    fn firewall_remove() -> io::Result<()> {
        in_txn(|h| {
            delete_filters(h)?;
            fwp("FwpmSubLayerDeleteByKey0", unsafe { wfp::FwpmSubLayerDeleteByKey0(h, &SUBLAYER) }, &[FWP_E_SUBLAYER_NOT_FOUND, FWP_E_NOT_FOUND])?;
            fwp("FwpmProviderDeleteByKey0", unsafe { wfp::FwpmProviderDeleteByKey0(h, &PROVIDER) }, &[FWP_E_PROVIDER_NOT_FOUND, FWP_E_NOT_FOUND])
        })
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
        std::fs::create_dir_all(t.join("elsewhere")).unwrap();
        for f in [home.join(".cargo/credentials.toml"), home.join(".cargo/.package-cache"), proj.join(".git/config"), proj.join(".env"), t.join("elsewhere/.netrc")] {
            std::fs::write(f, "x").unwrap();
        }
        let p = Policy {
            write: vec![proj.clone(), t.join("missing")],
            deny_read: vec![home.join(".ssh"), home.join(".cargo/credentials.toml"), home.join(".rustup"), proj.join(".env"),
                            t.join("elsewhere/.netrc")],
            protect: vec![proj.join(".git/hooks"), proj.join(".git/config"), proj.join(".qh"), home.join(".bashrc")],
            ..Default::default()
        };
        let got: Vec<(Kind, String, bool)> = plan(&p, &home, &home).into_iter()
            .map(|(k, path, d)| (k, path.strip_prefix(&t).unwrap().to_string_lossy().replace('\\', "/"), d)).collect();
        let want: Vec<(Kind, String, bool)> = [
            (Kind::Write, "proj", true),
            (Kind::Read, "home/.cargo", true),
            (Kind::Read, "home/AppData/Roaming/npm", true),
            (Kind::Write, "home/.cargo/.package-cache", false),
            (Kind::DenyRead, "home/.cargo/credentials.toml", false),
            (Kind::DenyRead, "proj/.env", false),
            (Kind::DenyRead, "elsewhere/.netrc", false), // outside the profile: Users may read it
            (Kind::DenyWrite, "proj/.git/hooks", true),
            (Kind::DenyDelete, "proj/.git", true),
            (Kind::DenyWrite, "proj/.git/config", false),
        ].iter().map(|(k, p, d)| (*k, p.to_string(), *d)).collect();
        assert_eq!(got, want);
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn sandbox_windows_password_and_git_trust() {
        let p = password(&[200; 32]);
        assert!(p.len() == 36 && p.chars().any(|c| c.is_ascii_uppercase()) && p.chars().any(|c| c.is_ascii_lowercase())
            && p.chars().any(|c| c.is_ascii_digit()) && p.chars().any(|c| !c.is_ascii_alphanumeric()));
        assert_eq!(git_trust(None)[2], ("GIT_CONFIG_COUNT".into(), "1".into()));
        let t = git_trust(Some("2"));
        assert_eq!((t[0].0.as_str(), t[1].1.as_str(), t[2].1.as_str()), ("GIT_CONFIG_KEY_2", "*", "3"));
    }
}
