//! The few OS services the terminal needs: per-user folders, the secret store, the local UTC
//! offset and the process's resident memory. macOS, Windows and Linux.
use anyhow::{bail, Result};
use std::path::PathBuf;

fn env(k: &str) -> Option<PathBuf> { std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from) }

/// Settings, favorites. macOS ~/Library/Application Support/TerminalOne, Windows
/// %APPDATA%\TerminalOne, Linux $XDG_CONFIG_HOME/terminal-one.
pub fn data_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") { env("HOME").map(|h| h.join("Library/Application Support/TerminalOne")) }
    else if cfg!(windows) { env("APPDATA").map(|d| d.join("TerminalOne")) }
    else { env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config"))).map(|d| d.join("terminal-one")) }
}

/// History, coin logos, rate-limit pauses. macOS ~/Library/Caches/TerminalOne, Windows
/// %LOCALAPPDATA%\TerminalOne\Cache, Linux $XDG_CACHE_HOME/terminal-one.
pub fn cache_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") { env("HOME").map(|h| h.join("Library/Caches/TerminalOne")) }
    else if cfg!(windows) { env("LOCALAPPDATA").map(|d| d.join("TerminalOne").join("Cache")) }
    else { env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|h| h.join(".cache"))).map(|d| d.join("terminal-one")) }
}

/// Crash logs. macOS ~/Library/Logs/TerminalOne, elsewhere a Logs folder next to the cache.
pub fn log_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") { env("HOME").map(|h| h.join("Library/Logs/TerminalOne")) }
    else { cache_dir().map(|d| d.join("Logs")) }
}

// ---------------------------------------------------------------- secret store
// One item per (service "terminal-one", account). macOS: Keychain through the `security` tool
// (items it creates have it on their access list: no prompt after every ad-hoc rebuild).
// Windows: Credential Manager, target "terminal-one:<account>". Linux: libsecret's `secret-tool`.

#[cfg(target_os = "macos")]
pub fn secret_get(account: &str) -> Option<String> {
    let out = std::process::Command::new("security").args(["find-generic-password", "-s", "terminal-one", "-a", account, "-w"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

/// The command (with the secret) goes over stdin, never into process arguments where `ps` sees it.
#[cfg(target_os = "macos")]
pub fn secret_set(account: &str, value: &str) -> Result<()> {
    use std::io::Write;
    if value.contains(['\n', '\r', '"', '\\']) { bail!("unsupported character in the value"); }
    let mut child = std::process::Command::new("security").arg("-i").stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn()?;
    child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?
        .write_all(format!("add-generic-password -U -s terminal-one -a \"{account}\" -w \"{value}\"\n").as_bytes())?;
    let out = child.wait_with_output()?;
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || err.contains("rror") { bail!("Keychain: {}", err.trim()); }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn secret_delete(account: &str) {
    let _ = std::process::Command::new("security").args(["delete-generic-password", "-s", "terminal-one", "-a", account]).output();
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }

#[cfg(windows)]
pub fn secret_get(account: &str) -> Option<String> {
    use windows_sys::Win32::Security::Credentials::{CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC};
    let target = wide(&format!("terminal-one:{account}"));
    let mut p: *mut CREDENTIALW = std::ptr::null_mut();
    // SAFETY: target is NUL-terminated; on success p points to a CREDENTIALW we free once.
    unsafe {
        if CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut p) == 0 { return None; }
        let c = &*p;
        let v = String::from_utf8_lossy(std::slice::from_raw_parts(c.CredentialBlob, c.CredentialBlobSize as usize)).trim().to_string();
        CredFree(p as _);
        Some(v).filter(|s| !s.is_empty())
    }
}

#[cfg(windows)]
pub fn secret_set(account: &str, value: &str) -> Result<()> {
    use windows_sys::Win32::Security::Credentials::{CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC};
    let mut target = wide(&format!("terminal-one:{account}"));
    let mut user = wide(account);
    let mut blob = value.as_bytes().to_vec();
    // SAFETY: every pointer outlives the call; zeroed fields are valid "absent" values.
    let ok = unsafe {
        let mut c: CREDENTIALW = std::mem::zeroed();
        c.Type = CRED_TYPE_GENERIC;
        c.TargetName = target.as_mut_ptr();
        c.UserName = user.as_mut_ptr();
        c.CredentialBlob = blob.as_mut_ptr();
        c.CredentialBlobSize = blob.len() as u32;
        c.Persist = CRED_PERSIST_LOCAL_MACHINE;
        CredWriteW(&c, 0)
    };
    if ok == 0 { bail!("Credential Manager: {}", std::io::Error::last_os_error()); }
    Ok(())
}

#[cfg(windows)]
pub fn secret_delete(account: &str) {
    use windows_sys::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};
    let target = wide(&format!("terminal-one:{account}"));
    // SAFETY: NUL-terminated target
    unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn secret_get(account: &str) -> Option<String> {
    let out = std::process::Command::new("secret-tool").args(["lookup", "service", "terminal-one", "account", account]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

/// `secret-tool store` reads the secret from stdin.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn secret_set(account: &str, value: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("secret-tool")
        .args(["store", "--label", &format!("terminal-one {account}"), "service", "terminal-one", "account", account])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn()
        .map_err(|e| anyhow::anyhow!("secret-tool (libsecret) not available: {e}"))?;
    child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?.write_all(value.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() { bail!("secret-tool: {}", String::from_utf8_lossy(&out.stderr).trim()); }
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
pub fn secret_delete(account: &str) {
    let _ = std::process::Command::new("secret-tool").args(["clear", "service", "terminal-one", "account", account]).output();
}

// ---------------------------------------------------------------- clock, memory

/// Local UTC offset in ms (std has no timezone API). Read once by callers.
#[cfg(unix)]
pub fn utc_offset_ms() -> i64 {
    let s = std::process::Command::new("date").arg("+%z").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let n: i64 = s.get(1..).and_then(|x| x.parse().ok()).unwrap_or(0);
    let ms = (n / 100 * 60 + n % 100) * 60_000;
    if s.starts_with('-') { -ms } else { ms }
}

#[cfg(windows)]
pub fn utc_offset_ms() -> i64 {
    use windows_sys::Win32::System::Time::{GetTimeZoneInformation, TIME_ZONE_INFORMATION};
    // SAFETY: plain out-parameter
    let (id, tz) = unsafe { let mut tz: TIME_ZONE_INFORMATION = std::mem::zeroed(); (GetTimeZoneInformation(&mut tz), tz) };
    // Bias is UTC - local in minutes; 2 = daylight time in effect
    let bias = tz.Bias + if id == 2 { tz.DaylightBias } else { tz.StandardBias };
    -(bias as i64) * 60_000
}

/// Resident memory of this process in MB.
#[cfg(unix)]
pub fn rss_mb() -> Option<u64> {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok().map(|kb| kb / 1024)
}

#[cfg(windows)]
pub fn rss_mb() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: out-parameter of the declared size
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        (GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) != 0).then(|| c.WorkingSetSize as u64 / (1024 * 1024))
    }
}

// ---------------------------------------------------------------- disk use

fn walk(dir: &std::path::Path, f: &mut impl FnMut(&std::path::Path, &std::fs::Metadata)) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(m) = e.metadata() else { continue };
        if m.is_dir() { walk(&e.path(), f) } else { f(&e.path(), &m) }
    }
}

/// Bytes used by the cache and log folders.
pub fn cache_bytes() -> u64 {
    let mut n = 0;
    for d in [cache_dir(), log_dir()].into_iter().flatten() { walk(&d, &mut |_, m| n += m.len()); }
    n
}

/// Delete everything in the cache and log folders except the rate-limit pauses (a cleared ban
/// record would let the next requests extend an active IP ban). Returns the bytes freed.
pub fn clear_cache() -> u64 {
    let mut n = 0;
    for d in [cache_dir(), log_dir()].into_iter().flatten() {
        walk(&d, &mut |p, m| {
            if p.file_name().is_some_and(|f| f == "rate-pauses.json") { return; }
            if std::fs::remove_file(p).is_ok() { n += m.len(); }
        });
    }
    n
}

/// Drop cache files untouched for `days` (history of symbols no longer viewed).
pub fn prune_cache(days: u64) {
    let Some(d) = cache_dir() else { return };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
    walk(&d, &mut |p, m| {
        if p.file_name().is_some_and(|f| f == "rate-pauses.json") { return; }
        if m.modified().is_ok_and(|t| t < cutoff) { let _ = std::fs::remove_file(p); }
    });
}

/// Append to a log file, rotating it to `<name>.old` past 1 MB so it never grows without bound.
pub fn append_log(name: &str, text: &str) {
    use std::io::Write;
    let Some(dir) = log_dir() else { return };
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join(name);
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 1 << 20) { let _ = std::fs::rename(&p, dir.join(format!("{name}.old"))); }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) { let _ = f.write_all(text.as_bytes()); }
}
