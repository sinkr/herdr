//! Integration tests for the detached server daemon's signal behavior.
//!
//! The auto-start path spawns `herdr server` in its own session so it
//! survives the terminal (and the attach client) that started it. These
//! tests prove the running daemon really is immune to SIGHUP: the server's
//! ctrlc handler registers SIGINT/SIGTERM/SIGHUP together, and without the
//! detached-daemon shield a stray hangup gracefully shuts the server down,
//! killing every pane.

#![cfg(unix)]

mod support;

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use support::{
    cleanup_test_base, register_runtime_dir, register_spawned_herdr_pid,
    unregister_spawned_herdr_pid, wait_for_socket,
};

fn unique_test_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    PathBuf::from(format!(
        "/tmp/herdr-daemon-test-{}-{nanos}",
        std::process::id()
    ))
}

/// Spawns `herdr server` the way the auto-start daemon path does: in its own
/// session with stdio detached. Deliberately does NOT pre-ignore SIGHUP so
/// the test exercises the server's own in-process shield rather than the
/// disposition inherited from the spawner.
fn spawn_detached_server(config_home: &Path, runtime_dir: &Path, api_socket_path: &Path) -> Child {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    register_runtime_dir(runtime_dir);
    fs::write(
        config_home.join("herdr/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command
        .arg("server")
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("HERDR_SOCKET_PATH", api_socket_path)
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env("SHELL", "/bin/sh")
        .env_remove("HERDR_ENV")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn().unwrap();
    register_spawned_herdr_pid(Some(child.id()));
    child
}

fn send_request(socket_path: &Path, request: &str) -> String {
    let mut stream = UnixStream::connect(socket_path).expect("should connect to API socket");
    writeln!(stream, "{request}").unwrap();

    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).unwrap();
    response.trim().to_string()
}

fn stop_server_and_reap(child: &mut Child, api_socket_path: &Path) {
    let _ = UnixStream::connect(api_socket_path).map(|mut stream| {
        let _ = writeln!(
            stream,
            r#"{{"id":"stop","method":"server.stop","params":{{}}}}"#
        );
        let mut response = String::new();
        let _ = BufReader::new(stream).read_line(&mut response);
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    unregister_spawned_herdr_pid(Some(child.id()));
}

/// A detached (auto-started) server daemon must survive SIGHUP and keep
/// serving. Before the shield, the ctrlc "termination" handler turned the
/// hangup into a graceful shutdown that killed every pane.
#[test]
fn detached_server_daemon_survives_sighup_and_keeps_serving() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket_path = runtime_dir.join("herdr.sock");

    let mut child = spawn_detached_server(&config_home, &runtime_dir, &api_socket_path);
    wait_for_socket(&api_socket_path, Duration::from_secs(15));

    let response = send_request(
        &api_socket_path,
        r#"{"id":"1","method":"ping","params":{}}"#,
    );
    assert!(
        response.contains("version"),
        "ping before SIGHUP should return version, got: {response}"
    );

    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGHUP) },
        0,
        "sending SIGHUP to the server daemon should succeed"
    );

    // Give a broken server time to run its graceful shutdown.
    thread::sleep(Duration::from_millis(750));

    assert!(
        child.try_wait().unwrap().is_none(),
        "detached server daemon must survive SIGHUP"
    );
    let response = send_request(
        &api_socket_path,
        r#"{"id":"2","method":"ping","params":{}}"#,
    );
    assert!(
        response.contains("version"),
        "ping after SIGHUP should still return version, got: {response}"
    );

    stop_server_and_reap(&mut child, &api_socket_path);
    cleanup_test_base(&base);
}

/// SIGTERM must still shut the detached daemon down cleanly: only SIGHUP is
/// shielded, direct termination keeps working for operators and updates.
#[test]
fn detached_server_daemon_still_stops_on_sigterm() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket_path = runtime_dir.join("herdr.sock");

    let mut child = spawn_detached_server(&config_home, &runtime_dir, &api_socket_path);
    wait_for_socket(&api_socket_path, Duration::from_secs(15));

    assert_eq!(
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
        0,
        "sending SIGTERM to the server daemon should succeed"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let exited = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            _ => break false,
        }
    };
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    unregister_spawned_herdr_pid(Some(child.id()));
    assert!(exited, "server daemon must exit gracefully on SIGTERM");

    cleanup_test_base(&base);
}
