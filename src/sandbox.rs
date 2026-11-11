use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OUT_CAP: u64 = 64 * 1024;
const MEM_KB: u64 = 4_194_304;
const ULIMIT_MEMORY: bool = cfg!(target_os = "linux");
const LIMITS: &str = if ULIMIT_MEMORY {
    "{ ulimit -t 120 && ulimit -v 4194304 && ulimit -f 1048576 && ulimit -n 256; } || exit 125; "
} else {
    "{ ulimit -t 120 && ulimit -f 1048576 && ulimit -n 256; } || exit 125; "
};

pub fn run(ws: &Path, tmp: &Path, cmd: &str, limit: Duration) -> Result<String, String> {
    std::fs::create_dir_all(tmp).map_err(|e| e.to_string())?;
    let ws = &ws.canonicalize().map_err(|e| format!("workspace: {e}"))?;
    let tmp = &tmp.canonicalize().map_err(|e| format!("tmp: {e}"))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = Command::new(exe)
        .arg("sandbox-exec")
        .arg(ws)
        .arg(tmp)
        .arg(cmd)
        .current_dir(ws)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )
        .env("HOME", tmp)
        .env("TMPDIR", tmp)
        .env(
            "NEXY_ENGINE_ALLOW_UNSANDBOXED",
            std::env::var("NEXY_ENGINE_ALLOW_UNSANDBOXED").unwrap_or_default(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("could not start the command: {e}"))?;
    let pid = child.id();
    let take = |r: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(r) = r {
                let _ = r.take(OUT_CAP).read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = take(
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let err = take(
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    );
    let t0 = Instant::now();
    let mut checked = t0;
    let mut over_memory = false;
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(s);
        }
        if !ULIMIT_MEMORY && checked.elapsed() >= Duration::from_secs(1) {
            checked = Instant::now();
            over_memory = group_rss_kb(pid) > MEM_KB;
        }
        if over_memory || t0.elapsed() > limit {
            kill_group(pid);
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // nothing it started outlives the call: background children would hold its output open and keep acting
    kill_group(pid);
    let (o, e) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    let code = match status {
        Some(s) => format!(
            "exit {}",
            s.code().map_or("signal".into(), |c| c.to_string())
        ),
        None if over_memory => format!("killed: over the {} MB memory limit", MEM_KB / 1024),
        None => format!("killed after {}s", limit.as_secs()),
    };
    Ok(format!(
        "[{code}]\n{o}{}{e}",
        if e.is_empty() { "" } else { "\n[stderr]\n" }
    ))
}

fn kill_group(pgid: u32) {
    if let Some(p) = rustix::process::Pid::from_raw(pgid as i32) {
        let _ = rustix::process::kill_process_group(p, rustix::process::Signal::KILL);
    }
}

fn group_rss_kb(pgid: u32) -> u64 {
    let Ok(out) = Command::new("ps").args(["-A", "-o", "pgid=,rss="]).output() else {
        return 0;
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let g: u32 = f.next()?.parse().ok()?;
            let kb: u64 = f.next()?.parse().ok()?;
            (g == pgid).then_some(kb)
        })
        .sum()
}

fn uid(tmp: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    let probe = tmp.join(".uid");
    let uid = std::fs::write(&probe, b"")
        .and_then(|_| std::fs::metadata(&probe))
        .map(|m| m.uid());
    let _ = std::fs::remove_file(&probe);
    uid
}

pub fn exec_confined(ws: &str, tmp: &str, cmd: &str) -> i32 {
    match uid(Path::new(tmp)) {
        Err(e) => {
            eprintln!("refused: cannot use the temp folder {tmp}: {e}");
            return 126;
        }
        Ok(0) if std::env::var("NEXY_ENGINE_ALLOW_ROOT").as_deref() != Ok("1") => {
            eprintln!("refused: commands do not run as root; run the engine as an ordinary user");
            return 126;
        }
        Ok(_) => {}
    }
    let script = format!("{LIMITS}{cmd}");
    let mut c = match confine(Path::new(ws), Path::new(tmp), &script) {
        Ok(c) => c,
        Err(why) => {
            if std::env::var("NEXY_ENGINE_ALLOW_UNSANDBOXED").as_deref() != Ok("1") {
                eprintln!("refused: {why}");
                return 126;
            }
            eprintln!("warning: running without a sandbox ({why})");
            let mut c = Command::new("sh");
            c.arg("-c").arg(&script);
            c
        }
    };
    let e = c.exec();
    eprintln!("could not start the sandboxed shell: {e}");
    127
}

fn read_roots(base: &[&str], ws: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = base.iter().map(PathBuf::from).collect();
    if let Ok(path) = std::env::var("PATH") {
        v.extend(
            std::env::split_paths(&path)
                .filter_map(|p| p.canonicalize().ok())
                .filter(|p| !ws.starts_with(p)),
        );
    }
    v.retain(|p| p.exists());
    v
}

#[cfg(target_os = "linux")]
fn confine(ws: &Path, tmp: &Path, script: &str) -> Result<Command, String> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        Scope, path_beneath_rules,
    };
    let abi = ABI::V6;
    let roots = read_roots(
        &[
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/etc",
            "/nix/store",
            "/run/current-system",
            "/opt",
            "/dev",
        ],
        &ws.canonicalize().map_err(|e| format!("workspace: {e}"))?,
    );
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.handle_access(AccessNet::from_all(abi)))
        .and_then(|r| r.scope(Scope::from_all(abi)))
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(roots, AccessFs::from_read(abi))))
        .and_then(|r| {
            r.add_rules(path_beneath_rules(
                [ws, tmp, Path::new("/dev/null")],
                AccessFs::from_all(abi),
            ))
        })
        .and_then(|r| r.restrict_self())
        .map_err(|e| format!("sandbox setup failed: {e}"))?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err("Landlock is not available on this kernel".into());
    }
    // below ABI 4 a command could open any TCP connection; below 6 it could signal the engine's other processes
    if ABI::from(status.landlock) < ABI::V4 {
        return Err(
            "this kernel's Landlock cannot block network connections (needs Linux 6.7 or newer)"
                .into(),
        );
    }
    let mut c = Command::new("sh");
    c.arg("-c").arg(script);
    Ok(c)
}

#[cfg(target_os = "macos")]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

#[cfg(target_os = "macos")]
fn confine(ws: &Path, tmp: &Path, script: &str) -> Result<Command, String> {
    if !Path::new(SANDBOX_EXEC).exists() {
        return Err("sandbox-exec is missing".into());
    }
    let ws = ws.canonicalize().map_err(|e| format!("workspace: {e}"))?;
    let tmp = tmp.canonicalize().map_err(|e| format!("tmp: {e}"))?;
    let mut c = Command::new(SANDBOX_EXEC);
    c.arg("-p")
        .arg(profile(&ws, &tmp))
        .arg("/bin/sh")
        .arg("-c")
        .arg(script);
    Ok(c)
}

#[cfg(target_os = "macos")]
fn profile(ws: &Path, tmp: &Path) -> String {
    let q = |p: &Path| {
        let s = p.to_string_lossy();
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    };
    let reads: Vec<String> = read_roots(
        &[
            "/usr",
            "/bin",
            "/sbin",
            "/System",
            "/Library/Developer/CommandLineTools",
            "/Applications/Xcode.app/Contents",
            "/opt/homebrew",
            "/nix/store",
            "/private/etc",
            "/private/var/db/timezone",
            "/private/var/db/dyld",
            "/dev",
        ],
        ws,
    )
    .iter()
    .map(|p| format!("(subpath {})", q(p)))
    .collect();
    let own = format!("(subpath {}) (subpath {})", q(ws), q(tmp));
    format!(
        "(version 1)
(deny default)
(allow process-exec process-fork)
(allow signal (target same-sandbox))
(allow sysctl-read)
(allow file-read-metadata)
(allow file-read* {} (literal \"/\") (literal \"/private/var/select/sh\") (literal \"/private/var/db/xcode_select_link\") (literal \"/Library/Preferences/com.apple.dt.Xcode.plist\") {own})
(allow file-write* {own} (literal \"/dev/null\") (literal \"/dev/tty\"))
(allow file-ioctl (literal \"/dev/null\") (literal \"/dev/tty\"))
(allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))
",
        reads.join(" ")
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn measures_a_process_group() {
        let me = std::process::id().to_string();
        let out = std::process::Command::new("ps")
            .args(["-o", "pgid=", "-p", &me])
            .output()
            .unwrap();
        let pgid: u32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        assert!(super::group_rss_kb(pgid) > 0);
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn confine(_: &Path, _: &Path, _: &str) -> Result<Command, String> {
    Err("no command sandbox on this OS yet".into())
}
