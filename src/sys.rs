//! The few OS services the terminal needs: per-user folders, the secret store, the local UTC
//! offset, the process's resident memory, notifications and sounds. macOS, Windows and Linux.
use anyhow::{bail, Result};
use std::path::PathBuf;

fn env(k: &str) -> Option<PathBuf> { std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from) }

/// Settings, favorites. macOS ~/Library/Application Support/Depth, Windows
/// %APPDATA%\Depth, Linux $XDG_CONFIG_HOME/depth.
pub fn data_dir() -> Option<PathBuf> {
    migrate();
    if cfg!(target_os = "macos") { env("HOME").map(|h| h.join("Library/Application Support/Depth")) }
    else if cfg!(windows) { env("APPDATA").map(|d| d.join("Depth")) }
    else { env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config"))).map(|d| d.join("depth")) }
}

/// History, coin logos, rate-limit pauses. macOS ~/Library/Caches/Depth, Windows
/// %LOCALAPPDATA%\Depth\Cache, Linux $XDG_CACHE_HOME/depth.
pub fn cache_dir() -> Option<PathBuf> {
    migrate();
    if cfg!(target_os = "macos") { env("HOME").map(|h| h.join("Library/Caches/Depth")) }
    else if cfg!(windows) { env("LOCALAPPDATA").map(|d| d.join("Depth").join("Cache")) }
    else { env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|h| h.join(".cache"))).map(|d| d.join("depth")) }
}

/// Crash logs. macOS ~/Library/Logs/Depth, elsewhere a Logs folder next to the cache.
pub fn log_dir() -> Option<PathBuf> {
    // T1_LOG_DIR: cargo test / cargo run write here (.cargo/config.toml), never into the installed app's logs
    if let Some(d) = env("T1_LOG_DIR") { return Some(d); }
    migrate();
    if cfg!(target_os = "macos") { env("HOME").map(|h| h.join("Library/Logs/Depth")) }
    else { cache_dir().map(|d| d.join("Logs")) }
}

/// The app was called Terminal One: move its folders to the new names once (only if the new one
/// does not exist yet). Logs on macOS live in their own folder; elsewhere they are inside the cache.
fn migrate() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let pairs: Vec<(Option<PathBuf>, &str, &str)> = if cfg!(target_os = "macos") {
            let h = env("HOME");
            vec![(h.clone(), "Library/Application Support/TerminalOne", "Library/Application Support/Depth"),
                 (h.clone(), "Library/Caches/TerminalOne", "Library/Caches/Depth"), (h, "Library/Logs/TerminalOne", "Library/Logs/Depth")]
        } else if cfg!(windows) {
            vec![(env("APPDATA"), "TerminalOne", "Depth"), (env("LOCALAPPDATA"), "TerminalOne", "Depth")]
        } else {
            vec![(env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config"))), "terminal-one", "depth"),
                 (env("XDG_CACHE_HOME").or_else(|| env("HOME").map(|h| h.join(".cache"))), "terminal-one", "depth")]
        };
        for (base, old, new) in pairs {
            let Some(b) = base else { continue };
            if b.join(old).exists() && !b.join(new).exists() { let _ = std::fs::rename(b.join(old), b.join(new)); }
        }
    });
}

// ---------------------------------------------------------------- secret store
// One item per (service "depth", account). Items of the old service "terminal-one" are moved
// over on first read; set/delete clear the old item so a removed key cannot come back from it. macOS: Keychain through the `security` tool
// (items it creates have it on their access list: no prompt after every ad-hoc rebuild).
// Windows: Credential Manager, target "depth:<account>". Linux: libsecret's `secret-tool`.

#[cfg(target_os = "macos")]
fn get(svc: &str, account: &str) -> Option<String> {
    let out = std::process::Command::new("security").args(["find-generic-password", "-s", svc, "-a", account, "-w"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

/// The command (with the secret) goes over stdin, never into process arguments where `ps` sees it.
#[cfg(target_os = "macos")]
fn set(svc: &str, account: &str, value: &str) -> Result<()> {
    use std::io::Write;
    if value.contains(['\n', '\r', '"', '\\']) { bail!("unsupported character in the value"); }
    let mut child = std::process::Command::new("security").arg("-i").stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn()?;
    child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?
        .write_all(format!("add-generic-password -U -s {svc} -a \"{account}\" -w \"{value}\"\n").as_bytes())?;
    let out = child.wait_with_output()?;
    let err = String::from_utf8_lossy(&out.stderr);
    if !out.status.success() || err.contains("rror") { bail!("Keychain: {}", err.trim()); }
    Ok(())
}

#[cfg(target_os = "macos")]
fn del(svc: &str, account: &str) {
    let _ = std::process::Command::new("security").args(["delete-generic-password", "-s", svc, "-a", account]).output();
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() }

#[cfg(windows)]
fn get(svc: &str, account: &str) -> Option<String> {
    use windows_sys::Win32::Security::Credentials::{CredFree, CredReadW, CREDENTIALW, CRED_TYPE_GENERIC};
    let target = wide(&format!("{svc}:{account}"));
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
fn set(svc: &str, account: &str, value: &str) -> Result<()> {
    use windows_sys::Win32::Security::Credentials::{CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC};
    let mut target = wide(&format!("{svc}:{account}"));
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
fn del(svc: &str, account: &str) {
    use windows_sys::Win32::Security::Credentials::{CredDeleteW, CRED_TYPE_GENERIC};
    let target = wide(&format!("{svc}:{account}"));
    // SAFETY: NUL-terminated target
    unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
}

#[cfg(not(any(target_os = "macos", windows)))]
fn get(svc: &str, account: &str) -> Option<String> {
    let out = std::process::Command::new("secret-tool").args(["lookup", "service", svc, "account", account]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
}

/// `secret-tool store` reads the secret from stdin.
#[cfg(not(any(target_os = "macos", windows)))]
fn set(svc: &str, account: &str, value: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new("secret-tool")
        .args(["store", "--label", &format!("{svc} {account}"), "service", svc, "account", account])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped()).spawn()
        .map_err(|e| anyhow::anyhow!("secret-tool (libsecret) not available: {e}"))?;
    child.stdin.take().ok_or_else(|| anyhow::anyhow!("no stdin"))?.write_all(value.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() { bail!("secret-tool: {}", String::from_utf8_lossy(&out.stderr).trim()); }
    Ok(())
}

#[cfg(not(any(target_os = "macos", windows)))]
fn del(svc: &str, account: &str) {
    let _ = std::process::Command::new("secret-tool").args(["clear", "service", svc, "account", account]).output();
}

const OLD_SERVICE: &str = "terminal-one";

pub fn secret_get(account: &str) -> Option<String> {
    get("depth", account).or_else(|| {
        let v = get(OLD_SERVICE, account)?;
        if set("depth", account, &v).is_ok() { del(OLD_SERVICE, account); }
        Some(v)
    })
}

pub fn secret_set(account: &str, value: &str) -> Result<()> {
    set("depth", account, value)?;
    del(OLD_SERVICE, account);
    Ok(())
}

pub fn secret_delete(account: &str) {
    del("depth", account);
    del(OLD_SERVICE, account);
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
    n + crate::db::hist_bytes()
}

/// Delete everything in the cache and log folders except the rate-limit pauses (a cleared ban
/// record would let the next requests extend an active IP ban). Returns the bytes freed.
pub fn clear_cache() -> u64 {
    let mut n = crate::db::hist_clear(None);
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
    crate::db::hist_clear(Some(days));
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

/// Panics (any thread, tokio tasks included) go to panic.log in `log_dir()` with location and
/// backtrace, then on to the previous hook (stderr in a terminal run): a Finder launch has no stderr.
pub fn install_panic_log() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let bt = std::backtrace::Backtrace::force_capture();
            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            append_log("panic.log", &format!("{ts} {info}\n{bt}\n"));
            prev(info);
        }));
    });
}

// ---------------------------------------------------------------- one instance

/// Take the exclusive lock on `p` (created if missing). The lock belongs to the open file, so a
/// second open of the same path fails even inside this process; the OS drops it when the file
/// closes or the process dies, so a crash never leaves a stale lock.
fn lock_file(p: &std::path::Path) -> std::io::Result<std::fs::File> {
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(p)?;
    f.try_lock().map_err(|e| match e { std::fs::TryLockError::WouldBlock => std::io::ErrorKind::WouldBlock.into(), std::fs::TryLockError::Error(e) => e })?;
    Ok(f)
}

/// One Depth per user: true for the first caller, which holds `data_dir()/depth.lock` until it
/// exits; false when another Depth runs (the caller says so and quits). A folder or filesystem
/// that cannot lock counts as free, never as a reason not to start. `T1_MULTI` (a dev build beside
/// the installed app) and screenshot runs (`T1_SHOT`) skip the check.
pub fn lock_instance() -> bool {
    static LOCK: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();
    if std::env::var_os("T1_MULTI").is_some() || std::env::var_os("T1_SHOT").is_some() { return true; }
    let Some(dir) = data_dir() else { return true };
    let _ = std::fs::create_dir_all(&dir);
    match lock_file(&dir.join("depth.lock")) {
        Ok(f) => { let _ = LOCK.set(f); true }
        Err(e) => e.kind() != std::io::ErrorKind::WouldBlock,
    }
}

// ---------------------------------------------------------------- notifications, sounds

/// Run a helper process detached from our stdio, reaped on a thread. A helper that is not
/// installed (notify-send, afplay on a stripped system) just means no notification / sound.
fn spawn_reaped(c: &mut std::process::Command) {
    let null = std::process::Stdio::null;
    if let Ok(mut c) = c.stdin(null()).stdout(null()).stderr(null()).spawn() { std::thread::spawn(move || { let _ = c.wait(); }); }
}

/// At most 10 notifications per 10 s (each one is a process): the 11th is dropped.
fn allow(now: std::time::Instant, recent: &mut std::collections::VecDeque<std::time::Instant>) -> bool {
    while recent.front().is_some_and(|t| now.duration_since(*t) >= std::time::Duration::from_secs(10)) { recent.pop_front(); }
    if recent.len() >= 10 { return false; }
    recent.push_back(now);
    true
}

/// An OS notification with a title line and a body: macOS Notification Center through osascript,
/// a Windows toast through PowerShell, Linux libnotify's `notify-send`. Never blocks; shown by the
/// plain egui build only (the native hosts post notifications themselves).
pub fn notify(title: &str, body: &str) {
    static RECENT: std::sync::Mutex<std::collections::VecDeque<std::time::Instant>> = std::sync::Mutex::new(std::collections::VecDeque::new());
    if !allow(std::time::Instant::now(), &mut RECENT.lock().unwrap()) { return eprintln!("[notify] too many notifications, dropped: {title}"); }
    #[cfg(target_os = "macos")]
    {
        let q = |s: &str| s.replace('\\', "").replace('"', "'");
        let script = format!("display notification \"{}\" with title \"Depth\" subtitle \"{}\"", q(body), q(title));
        spawn_reaped(std::process::Command::new("osascript").arg("-e").arg(script));
    }
    // ponytail: Windows PowerShell 5.1 (always present) posts the toast under its own app id, so the
    // sender reads "Windows PowerShell" and each toast costs ~0.5-1 s of process start. Upgrade: a
    // Start-menu shortcut carrying Depth's own AppUserModelID (the installer), then no PowerShell.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const TOAST: &str = "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; \
            [Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] | Out-Null; \
            $x = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); \
            $t = $x.GetElementsByTagName('text'); \
            $t.Item(0).AppendChild($x.CreateTextNode($env:DEPTH_T)) | Out-Null; \
            $t.Item(1).AppendChild($x.CreateTextNode($env:DEPTH_B)) | Out-Null; \
            [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe').Show([Windows.UI.Notifications.ToastNotification]::new($x))";
        // title and body travel in the environment, so no quoting can break the script
        spawn_reaped(std::process::Command::new("powershell").args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", TOAST])
            .env("DEPTH_T", title).env("DEPTH_B", body).creation_flags(0x0800_0000 /* CREATE_NO_WINDOW */));
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    spawn_reaped(std::process::Command::new("notify-send").args(["-a", "Depth", "--", title, body]));
}

/// Play a sound file (wav; aiff on macOS) without blocking: afplay, winmm `PlaySoundW`, or
/// PulseAudio's `paplay` with ALSA's `aplay` as the fallback.
pub fn play_sound_file(p: &std::path::Path) {
    #[cfg(target_os = "macos")]
    spawn_reaped(std::process::Command::new("afplay").arg(p));
    #[cfg(windows)]
    {
        use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_NODEFAULT};
        let w = wide(&p.to_string_lossy());
        // SAFETY: w is NUL-terminated and lives past the call; SND_FILENAME takes no module handle
        unsafe { PlaySoundW(w.as_ptr(), std::ptr::null_mut(), SND_FILENAME | SND_ASYNC | SND_NODEFAULT) };
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let p = p.to_path_buf();
        std::thread::spawn(move || {
            let run = |prog: &str| std::process::Command::new(prog).arg(&p).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
            if !run("paplay") { run("aplay"); }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    #[test]
    fn notifications_are_rate_limited() {
        let (t0, mut recent) = (Instant::now(), std::collections::VecDeque::new());
        assert!((0..10).all(|i| super::allow(t0 + Duration::from_millis(i), &mut recent)));
        assert!(!super::allow(t0 + Duration::from_secs(9), &mut recent));
        assert!(super::allow(t0 + Duration::from_secs(11), &mut recent));
    }

    #[test]
    fn one_holder_of_the_instance_lock() {
        let p = std::env::temp_dir().join(format!("depth-lock-test-{}", std::process::id()));
        let first = super::lock_file(&p).unwrap();
        assert_eq!(super::lock_file(&p).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        drop(first);
        assert!(super::lock_file(&p).is_ok());
        let _ = std::fs::remove_file(p);
    }
}
