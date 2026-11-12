use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const OUT_CAP: u64 = 64 * 1024;
const MEM_KB: u64 = 4_194_304;
const ULIMIT_MEMORY: bool = cfg!(target_os = "linux");
const LIMITS: &str = if ULIMIT_MEMORY {
    "{ ulimit -t 120 && ulimit -v 4194304 && ulimit -f 1048576 && ulimit -n 256; } || exit 125; "
} else {
    "{ ulimit -t 120 && ulimit -f 1048576 && ulimit -n 256; } || exit 125; "
};

/// Commands running at once across the engine; each turn runs its tools one at a time.
const MAX_RUNNING: usize = 4;
/// Processes (threads on Linux) a command may add to what its user already runs.
const PROCS_PER_COMMAND: u64 = 256;
/// How long a stopped command gets to clean up before its group is killed from outside.
const GRACE: Duration = Duration::from_secs(5);

static RUNNING: AtomicUsize = AtomicUsize::new(0);

struct Slot;

impl Slot {
    fn take() -> Result<Slot, String> {
        // the slot is held from here; Drop gives it back on refusal too
        let slot = Slot;
        if RUNNING.fetch_add(1, Ordering::SeqCst) >= MAX_RUNNING {
            return Err(format!(
                "{MAX_RUNNING} commands are already running; wait for one to finish"
            ));
        }
        Ok(slot)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::SeqCst);
    }
}

pub fn run(ws: &Path, tmp: &Path, cmd: &str, limit: Duration) -> Result<String, String> {
    let _slot = Slot::take()?;
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
        .env(SUPERVISED, "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("could not start the command: {e}"))?;
    let pid = child.id();
    // closing this pipe tells the supervisor to stop the command and everything it started
    let mut stop = child.stdin.take();
    let take = |r: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(r) = r
                && let Err(e) = r.take(OUT_CAP).read_to_end(&mut buf)
            {
                buf.extend_from_slice(format!("\n[output error: {e}]").as_bytes());
            }
            // the receiver is gone only when the call stopped waiting for this output
            if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                eprintln!("run: output arrived after the call returned");
            }
        });
        rx
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
    let mut stopped_at: Option<Instant> = None;
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break stopped_at.is_none().then_some(s);
        }
        if !ULIMIT_MEMORY && checked.elapsed() >= Duration::from_secs(1) {
            checked = Instant::now();
            over_memory |= group_rss_kb(pid) > MEM_KB;
        }
        match stopped_at {
            None if over_memory || t0.elapsed() > limit => {
                drop(stop.take());
                stopped_at = Some(Instant::now());
            }
            Some(at) if at.elapsed() > GRACE => {
                kill_group(pid);
                child.wait().map_err(|e| e.to_string())?;
                break None;
            }
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // the supervisor has ended the command's processes; this catches any left in its group
    kill_group(pid);
    let read = |rx: std::sync::mpsc::Receiver<String>| {
        rx.recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| "[output still held open by a process that escaped]".into())
    };
    let (o, e) = (read(out), read(err));
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

const SUPERVISED: &str = "NEXY_ENGINE_SUPERVISED";

fn kill_group(pgid: u32) {
    if let Some(p) = rustix::process::Pid::from_raw(pgid as i32) {
        match rustix::process::kill_process_group(p, rustix::process::Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {}
            Err(e) => eprintln!("run: cannot kill process group {pgid}: {e}"),
        }
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
    if let Err(e) = std::fs::remove_file(&probe) {
        eprintln!("warning: cannot remove {}: {e}", probe.display());
    }
    uid
}

/// Processes on macOS, threads on Linux: what RLIMIT_NPROC counts for this user.
#[cfg(target_os = "linux")]
fn user_tasks() -> Result<u64, String> {
    let me = rustix::process::getuid().as_raw().to_string();
    let rd = std::fs::read_dir("/proc").map_err(|e| format!("/proc: {e}"))?;
    let mut n = 0;
    for e in rd.flatten() {
        if !e
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
        {
            continue;
        }
        // a process can end while it is being read
        let Ok(st) = std::fs::read_to_string(e.path().join("status")) else {
            continue;
        };
        let field = |k: &str| {
            st.lines()
                .find_map(|l| l.strip_prefix(k))
                .and_then(|v| v.split_whitespace().next())
        };
        if field("Uid:") == Some(me.as_str()) {
            n += field("Threads:").and_then(|t| t.parse().ok()).unwrap_or(1);
        }
    }
    Ok(n)
}

#[cfg(not(target_os = "linux"))]
fn user_tasks() -> Result<u64, String> {
    let me = rustix::process::getuid().as_raw().to_string();
    let out = Command::new("ps")
        .args(["-A", "-o", "uid="])
        .output()
        .map_err(|e| format!("ps: {e}"))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.trim() == me)
        .count() as u64)
}

/// Caps the command's process count a little above what the user runs now, so a fork bomb stops there.
fn limit_processes() -> Result<u64, String> {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let used = user_tasks()?;
    let hard = getrlimit(Resource::Nproc).maximum;
    let cap = hard.map_or(used + PROCS_PER_COMMAND, |h| {
        h.min(used + PROCS_PER_COMMAND)
    });
    setrlimit(
        Resource::Nproc,
        Rlimit {
            current: Some(cap),
            maximum: Some(cap),
        },
    )
    .map_err(|e| format!("cannot limit processes: {e}"))?;
    Ok(cap)
}

/// A stop flag set when the engine closes our stdin (or never, when the engine is not watching).
fn stop_flag() -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    if std::env::var(SUPERVISED).as_deref() == Ok("1") {
        let s = stop.clone();
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            if let Err(e) = std::io::stdin().read_to_end(&mut sink) {
                eprintln!("supervisor: stdin: {e}");
            }
            s.store(true, Ordering::SeqCst);
        });
    }
    stop
}

#[cfg(target_os = "macos")]
fn code_of(s: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(9))
}

/// `sandbox-exec WS TMP CMD`: checks, limits, then runs the command under a supervisor that ends
/// every process it started, including ones that left its group or session.
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
    if let Err(e) = limit_processes() {
        eprintln!("refused: {e}");
        return 126;
    }
    let script = format!("{LIMITS}{cmd}");
    supervise(ws, tmp, &script)
}

/// Linux: this process is a subreaper, so anything the command starts, even after `setsid` and a
/// double fork, is re-parented here and can be found and killed. The command itself is confined in
/// a child (`sandbox-inner`); with Landlock signal scoping it cannot signal this process.
#[cfg(target_os = "linux")]
fn supervise(ws: &str, tmp: &str, script: &str) -> i32 {
    use rustix::process::{WaitOptions, set_child_subreaper, waitpid};
    if let Err(e) = set_child_subreaper(Some(rustix::process::getpid())) {
        eprintln!("refused: cannot become a subreaper: {e}");
        return 126;
    }
    let stop = stop_flag();
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("refused: {e}");
            return 126;
        }
    };
    let inner = match Command::new(exe)
        .args(["sandbox-inner", ws, tmp, script, "linux"])
        .env_remove(SUPERVISED)
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(c) => rustix::process::Pid::from_child(&c),
        Err(e) => {
            eprintln!("could not start the sandboxed shell: {e}");
            return 127;
        }
    };
    let mut code = 137;
    loop {
        match waitpid(None, WaitOptions::NOHANG) {
            Ok(Some((p, st))) if p == inner => {
                code = st
                    .exit_status()
                    .unwrap_or_else(|| 128 + st.terminating_signal().unwrap_or(9));
                break;
            }
            // an orphan the command left behind ended: reaped here
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(e) => {
                eprintln!("supervisor: wait: {e}");
                break;
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    end_children();
    code
}

/// Kills our children until none are left: each death re-parents its own children to us.
#[cfg(target_os = "linux")]
fn end_children() {
    use rustix::io::Errno;
    use rustix::process::{Signal, WaitOptions, kill_process, waitpid};
    let me = rustix::process::getpid().as_raw_pid();
    for _ in 0..1000 {
        let kids = match children_of(me) {
            Ok(k) => k,
            Err(e) => {
                eprintln!("supervisor: cannot list processes: {e}");
                return;
            }
        };
        for k in kids.into_iter().filter_map(rustix::process::Pid::from_raw) {
            match kill_process(k, Signal::KILL) {
                Ok(()) | Err(Errno::SRCH) => {}
                Err(e) => eprintln!("supervisor: kill {}: {e}", k.as_raw_pid()),
            }
        }
        loop {
            match waitpid(None, WaitOptions::NOHANG) {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(Errno::CHILD) => return,
                Err(e) => {
                    eprintln!("supervisor: wait: {e}");
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    eprintln!("warning: some processes the command started could not be stopped");
}

#[cfg(target_os = "linux")]
fn children_of(parent: i32) -> std::io::Result<Vec<i32>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir("/proc")?.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        // a process can end while it is being read
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        // "pid (comm) state ppid ...": comm may hold spaces or brackets
        let ppid = stat
            .rsplit_once(')')
            .and_then(|(_, r)| r.split_whitespace().nth(1))
            .and_then(|p| p.parse::<i32>().ok());
        if ppid == Some(parent) {
            v.push(pid);
        }
    }
    Ok(v)
}

/// `sandbox-inner WS TMP SCRIPT MODE` on Linux: confine this process, then become the shell.
#[cfg(target_os = "linux")]
pub fn inner(ws: &str, tmp: &str, script: &str, _mode: &str) -> i32 {
    let mut c = match confine(Path::new(ws), Path::new(tmp), script) {
        Ok(c) => c,
        Err(why) => match unsandboxed(&why, script) {
            Some(c) => c,
            None => return 126,
        },
    };
    let e = c.exec();
    eprintln!("could not start the sandboxed shell: {e}");
    127
}

#[cfg(target_os = "linux")]
fn unsandboxed(why: &str, script: &str) -> Option<Command> {
    if std::env::var("NEXY_ENGINE_ALLOW_UNSANDBOXED").as_deref() != Ok("1") {
        eprintln!("refused: {why}");
        return None;
    }
    eprintln!("warning: running without a sandbox ({why})");
    let mut c = Command::new("sh");
    c.arg("-c").arg(script);
    Some(c)
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
    let exe = std::env::current_exe()
        .and_then(|e| e.canonicalize())
        .map_err(|e| format!("engine path: {e}"))?;
    let mut c = Command::new(SANDBOX_EXEC);
    c.arg("-p")
        .arg(profile(&ws, &tmp, &exe))
        .arg(&exe)
        .args(["sandbox-inner"])
        .arg(&ws)
        .arg(&tmp)
        .arg(script)
        .arg("sandboxed");
    Ok(c)
}

/// macOS has no subreaper. The supervisor runs inside the command's sandbox instead, where the
/// profile lets a process signal only others in the same sandbox: at the end it signals every
/// process id, and only the command's own processes, wherever they moved, can receive it.
#[cfg(target_os = "macos")]
fn supervise(ws: &str, tmp: &str, script: &str) -> i32 {
    let mut c = match confine(Path::new(ws), Path::new(tmp), script) {
        Ok(c) => c,
        Err(why) => {
            if std::env::var("NEXY_ENGINE_ALLOW_UNSANDBOXED").as_deref() != Ok("1") {
                eprintln!("refused: {why}");
                return 126;
            }
            eprintln!("warning: running without a sandbox ({why})");
            match std::env::current_exe() {
                Ok(exe) => {
                    let mut c = Command::new(exe);
                    c.args(["sandbox-inner", ws, tmp, script, "open"]);
                    c
                }
                Err(e) => {
                    eprintln!("refused: {e}");
                    return 126;
                }
            }
        }
    };
    let e = c.exec();
    eprintln!("could not start the sandbox: {e}");
    127
}

/// `sandbox-inner WS TMP SCRIPT MODE` on macOS: runs inside the sandbox, starts the shell, waits,
/// then ends every process in this sandbox.
#[cfg(target_os = "macos")]
pub fn inner(_ws: &str, _tmp: &str, script: &str, mode: &str) -> i32 {
    // proof, taken once at start, that the sandbox stops us signalling the engine that started us
    let sweep = mode == "sandboxed" && sandbox_blocks_parent();
    if mode == "sandboxed" && !sweep {
        eprintln!("refused: the sandbox does not limit signals");
        return 126;
    }
    let stop = stop_flag();
    let mut child = match Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .env_remove(SUPERVISED)
        .stdin(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("could not start the sandboxed shell: {e}");
            return 127;
        }
    };
    let mut code = 137;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                code = code_of(s);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("supervisor: wait: {e}");
                break;
            }
        }
        if stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if sweep {
        end_sandbox();
    } else if let Err(e) = child.kill() {
        eprintln!("supervisor: kill: {e}");
    }
    if let Err(e) = child.wait() {
        eprintln!("supervisor: wait: {e}");
    }
    code
}

#[cfg(target_os = "macos")]
fn sandbox_blocks_parent() -> bool {
    rustix::process::getppid()
        .filter(|p| !p.is_init())
        .is_some_and(|p| rustix::process::test_kill_process(p) == Err(rustix::io::Errno::PERM))
}

/// Highest process id macOS hands out.
#[cfg(target_os = "macos")]
const PID_MAX: i32 = 99_999;

/// Stops, then kills, every process this sandbox may signal: only its own. Two passes of each
/// catch processes forked while a pass was under way.
#[cfg(target_os = "macos")]
fn end_sandbox() {
    use rustix::io::Errno;
    use rustix::process::{Pid, Signal, kill_process};
    let me = rustix::process::getpid().as_raw_pid();
    for sig in [Signal::STOP, Signal::STOP, Signal::KILL, Signal::KILL] {
        for p in (2..=PID_MAX).filter(|p| *p != me).filter_map(Pid::from_raw) {
            match kill_process(p, sig) {
                Ok(()) | Err(Errno::PERM) | Err(Errno::SRCH) => {}
                Err(e) => {
                    eprintln!("supervisor: signal {}: {e}", p.as_raw_pid());
                    return;
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn profile(ws: &Path, tmp: &Path, exe: &Path) -> String {
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
(deny signal (with no-report))
(allow signal (target same-sandbox))
(allow sysctl-read)
(allow file-read-metadata)
(allow file-read* {} (literal \"/\") (literal \"/private/var/select/sh\") (literal \"/private/var/db/xcode_select_link\") (literal \"/Library/Preferences/com.apple.dt.Xcode.plist\") (literal {}) {own})
(allow file-write* {own} (literal \"/dev/null\") (literal \"/dev/tty\"))
(allow file-ioctl (literal \"/dev/null\") (literal \"/dev/tty\"))
(allow mach-lookup (global-name \"com.apple.system.opendirectoryd.libinfo\"))
",
        reads.join(" "),
        q(exe)
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn at_most_four_commands_run_at_once() {
        let held: Vec<super::Slot> = (0..super::MAX_RUNNING)
            .map(|_| super::Slot::take().unwrap())
            .collect();
        let d = std::env::temp_dir();
        let e = super::run(
            &d,
            &d.join("ne-cap"),
            "true",
            std::time::Duration::from_secs(5),
        );
        assert!(e.is_err_and(|e| e.contains("already running")));
        drop(held);
        assert_eq!(super::RUNNING.load(super::Ordering::SeqCst), 0);
        assert!(super::Slot::take().is_ok());
    }

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
fn supervise(_: &str, _: &str, _: &str) -> i32 {
    eprintln!("refused: no command sandbox on this OS yet");
    126
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn inner(_: &str, _: &str, _: &str, _: &str) -> i32 {
    126
}
