#![cfg(unix)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

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
