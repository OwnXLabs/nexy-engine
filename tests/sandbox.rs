#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::Duration;

/// RLIMIT_NPROC counts every process of the user, so the fork bomb uses up the allowance of any
/// command running beside it. Commands share this lock; the bomb holds it alone.
static PROCS: RwLock<()> = RwLock::new(());

fn shared() -> RwLockReadGuard<'static, ()> {
    PROCS.read().unwrap_or_else(|e| e.into_inner())
}

fn alone() -> RwLockWriteGuard<'static, ()> {
    PROCS.write().unwrap_or_else(|e| e.into_inner())
}

struct Dirs {
    base: PathBuf,
    ws: PathBuf,
    tmp: PathBuf,
}

fn dirs(name: &str) -> Dirs {
    let base = std::env::temp_dir()
        .join(format!("ne-sbx-{name}-{}", std::process::id()))
        .canonicalize_or_self();
    let _ = std::fs::remove_dir_all(&base);
    let (ws, tmp) = (base.join("ws"), base.join("tmp"));
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::create_dir_all(base.join("outside")).unwrap();
    Dirs { base, ws, tmp }
}

trait Canon {
    fn canonicalize_or_self(self) -> PathBuf;
}

impl Canon for PathBuf {
    fn canonicalize_or_self(self) -> PathBuf {
        std::fs::create_dir_all(&self).unwrap();
        self.canonicalize().unwrap_or(self)
    }
}

fn confined(d: &Dirs, cmd: &str) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_nexy-engine"))
        .args(["sandbox-exec"])
        .arg(&d.ws)
        .arg(&d.tmp)
        .arg(cmd)
        .current_dir(&d.ws)
        .env_remove("NEXY_ENGINE_ALLOW_UNSANDBOXED")
        .env("HOME", &d.tmp)
        .env("TMPDIR", &d.tmp)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

fn unavailable(text: &str) -> bool {
    let off = text.contains("Landlock is not available");
    if off {
        eprintln!("skipped: no sandbox on this kernel ({text})");
    }
    off
}

#[test]
fn writes_inside_the_workspace() {
    let _g = shared();
    let d = dirs("in");
    let (code, text) = confined(&d, "echo nexy-$((6*7)) > made.txt && cat made.txt");
    if unavailable(&text) {
        return;
    }
    assert_eq!(code, 0, "{text}");
    assert!(text.contains("nexy-42"), "{text}");
    assert_eq!(
        std::fs::read_to_string(d.ws.join("made.txt")).unwrap(),
        "nexy-42\n"
    );
    let (code, text) = confined(&d, "echo t > \"$TMPDIR/t.txt\" && cat \"$TMPDIR/t.txt\"");
    assert_eq!(code, 0, "{text}");
    std::fs::remove_dir_all(&d.base).unwrap();
}

#[test]
fn cannot_write_or_read_outside() {
    let _g = shared();
    let d = dirs("out");
    let secret = d.base.join("outside/secret.txt");
    std::fs::write(&secret, "hidden").unwrap();
    let target = d.base.join("outside/escaped.txt");
    let (_, text) = confined(&d, &format!("echo x > {}", target.display()));
    if unavailable(&text) {
        return;
    }
    assert!(!target.exists(), "wrote outside the workspace: {text}");
    let (_, text) = confined(&d, "echo x > ../outside/escaped2.txt; touch ~/../../x");
    assert!(!d.base.join("outside/escaped2.txt").exists(), "{text}");
    let (_, text) = confined(&d, &format!("cat {}", secret.display()));
    assert!(
        !text.contains("hidden"),
        "read outside the workspace: {text}"
    );
    std::fs::remove_dir_all(&d.base).unwrap();
}

fn connects(listener: &TcpListener) -> bool {
    listener.set_nonblocking(true).unwrap();
    for _ in 0..20 {
        if let Ok((mut s, _)) = listener.accept() {
            let _ = s.write_all(b"HTTP/1.0 200 OK\r\n\r\nok");
            let mut b = [0u8; 64];
            let _ = s.read(&mut b);
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn no_network() {
    let _g = shared();
    let curl = ["/usr/bin/curl", "/bin/curl"]
        .into_iter()
        .find(|p| Path::new(p).exists());
    let Some(curl) = curl else {
        eprintln!("skipped: no curl to test with");
        return;
    };
    let d = dirs("net");
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let cmd = format!("{curl} -s -m 3 {url}");
    let control = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        connects(&listener),
        "control: curl could not connect unconfined"
    );
    let _ = control.wait_with_output();
    let (code, text) = confined(&d, &format!("{cmd}; echo rc=$?"));
    if unavailable(&text) {
        return;
    }
    assert_eq!(code, 0, "{text}");
    assert!(
        !connects(&listener),
        "a confined command reached the network"
    );
    assert!(!text.contains("rc=0"), "{text}");
    std::fs::remove_dir_all(&d.base).unwrap();
}

#[test]
fn nothing_outlives_the_command() {
    let _g = shared();
    let d = dirs("bg");
    let marker = d.ws.join("late.txt");
    let cmd = format!("(sleep 3; echo late > {}) & echo started", marker.display());
    let t0 = std::time::Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_nexy-engine"))
        .args(["sandbox-run"])
        .arg(&d.ws)
        .arg(&d.tmp)
        .arg(&cmd)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    if unavailable(&text) {
        return;
    }
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "a background child held the call open: {text}"
    );
    assert!(text.contains("started"), "{text}");
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        !marker.exists(),
        "a background child kept running after the call"
    );
    let _ = std::fs::remove_dir_all(&d.base);
}

fn via_run(d: &Dirs, cmd: &str) -> (String, Duration) {
    let t0 = std::time::Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_nexy-engine"))
        .args(["sandbox-run"])
        .arg(&d.ws)
        .arg(&d.tmp)
        .arg(cmd)
        .output()
        .unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    (text, t0.elapsed())
}

/// Live processes whose command line contains `marker`; killed so a failing run leaves nothing.
fn survivors(marker: &str) -> String {
    let out = Command::new("pgrep")
        .args(["-fl", marker])
        .output()
        .unwrap();
    let found = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !found.is_empty() {
        let k = Command::new("pkill")
            .args(["-9", "-f", marker])
            .status()
            .unwrap();
        assert!(k.success(), "could not clean up {marker}");
    }
    found
}

/// A new session and a double fork leave the process group; they must still end with the call.
#[test]
fn a_new_session_does_not_outlive_the_command() {
    let _g = shared();
    let d = dirs("sid");
    let marker = format!("sleep 30{}", std::process::id());
    let escape = if Path::new("/usr/bin/setsid").exists() || Path::new("/bin/setsid").exists() {
        format!("setsid {marker} &")
    } else {
        // macOS has no setsid command: perl does the same calls
        format!(
            "perl -e 'use POSIX; fork and exit; POSIX::setsid(); fork and exit; exec qw({marker})' &"
        )
    };
    let (text, took) = via_run(&d, &format!("{escape} sleep 1; echo started"));
    if unavailable(&text) {
        return;
    }
    assert!(text.contains("started"), "{text}");
    assert!(
        took < Duration::from_secs(20),
        "the call took {took:?}: {text}"
    );
    std::thread::sleep(Duration::from_millis(300));
    let left = survivors(&marker);
    assert!(left.is_empty(), "outlived the command: {left}\n{text}");
    let _ = std::fs::remove_dir_all(&d.base);
}

/// A bounded fork bomb (up to 2,047 processes) hits the per-command process limit and is cleaned up.
#[test]
fn a_fork_bomb_stops_at_the_process_limit() {
    let _g = alone();
    let d = dirs("bomb");
    let marker = format!("sleep 4.{}", std::process::id());
    let bomb = format!(
        "b() {{ if [ $1 -lt 10 ]; then b $(($1+1)) & b $(($1+1)) & fi; {marker}; }}; b 0; wait"
    );
    let (text, took) = via_run(&d, &bomb);
    if unavailable(&text) {
        return;
    }
    assert!(took < Duration::from_secs(30), "took {took:?}");
    assert!(
        text.to_lowercase().contains("fork") || text.contains("Resource temporarily unavailable"),
        "the bomb was not limited: {}",
        &text[..text.len().min(400)]
    );
    std::thread::sleep(Duration::from_millis(300));
    let left = survivors(&marker);
    assert!(
        left.is_empty(),
        "outlived the command: {} processes",
        left.lines().count()
    );
    let ok = Command::new("true").status().unwrap();
    assert!(ok.success(), "the caller could no longer start processes");
    let _ = std::fs::remove_dir_all(&d.base);
}
