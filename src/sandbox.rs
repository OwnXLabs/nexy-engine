use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const OUT_CAP: u64 = 64 * 1024;

pub fn run(ws: &Path, tmp: &Path, cmd: &str, limit: Duration) -> Result<String, String> {
    std::fs::create_dir_all(tmp).map_err(|e| e.to_string())?;
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
    let status = loop {
        if let Some(s) = child.try_wait().map_err(|e| e.to_string())? {
            break Some(s);
        }
        if t0.elapsed() > limit {
            let _ = Command::new("kill")
                .args(["-9", &format!("-{pid}")])
                .status();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (o, e) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    let code = match status {
        Some(s) => format!(
            "exit {}",
            s.code().map_or("signal".into(), |c| c.to_string())
        ),
        None => format!("killed after {}s", limit.as_secs()),
    };
    Ok(format!(
        "[{code}]\n{o}{}{e}",
        if e.is_empty() { "" } else { "\n[stderr]\n" }
    ))
}

const LIMITS: &str =
    "{ ulimit -t 120 && ulimit -v 4194304 && ulimit -f 1048576 && ulimit -n 256; } || exit 125; ";

fn is_root() -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid() == 0)
        .unwrap_or(false)
}

pub fn exec_confined(ws: &str, tmp: &str, cmd: &str) -> i32 {
    if is_root() && std::env::var("NEXY_ENGINE_ALLOW_ROOT").as_deref() != Ok("1") {
        eprintln!("refused: commands do not run as root; run the engine as an ordinary user");
        return 126;
    }
    if let Err(why) = confine(Path::new(ws), Path::new(tmp)) {
        if std::env::var("NEXY_ENGINE_ALLOW_UNSANDBOXED").as_deref() != Ok("1") {
            eprintln!("refused: {why}");
            return 126;
        }
        eprintln!("warning: running without a sandbox ({why})");
    }
    let e = Command::new("sh")
        .arg("-c")
        .arg(format!("{LIMITS}{cmd}"))
        .exec();
    eprintln!("could not run sh: {e}");
    127
}

#[cfg(target_os = "linux")]
fn confine(ws: &Path, tmp: &Path) -> Result<(), String> {
    use landlock::{
        ABI, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus,
        path_beneath_rules,
    };
    let abi = ABI::V5;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .and_then(|r| r.handle_access(AccessNet::from_all(abi)))
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(read_roots(), AccessFs::from_read(abi))))
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
    Ok(())
}

#[cfg(target_os = "linux")]
fn read_roots() -> Vec<std::path::PathBuf> {
    let mut v: Vec<std::path::PathBuf> = [
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
    ]
    .iter()
    .map(std::path::PathBuf::from)
    .collect();
    if let Ok(path) = std::env::var("PATH") {
        v.extend(std::env::split_paths(&path).filter_map(|p| p.canonicalize().ok()));
    }
    v.retain(|p| p.exists());
    v
}

#[cfg(not(target_os = "linux"))]
fn confine(_: &Path, _: &Path) -> Result<(), String> {
    Err("no command sandbox on this OS yet (Linux only)".into())
}
