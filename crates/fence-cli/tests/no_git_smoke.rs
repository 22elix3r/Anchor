#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

#[test]
fn capture_diff_restore_doctor_and_gc_need_no_git_executable() {
    let root = repository();
    fs::write(root.path().join("file"), b"base").unwrap();

    let run = fence(
        root.path(),
        &["run", "--", "/bin/sh", "-c", "printf session > file"],
    );
    assert_success("run", &run);
    let stderr = String::from_utf8(run.stderr).unwrap();
    let session = stderr
        .split_whitespace()
        .nth(2)
        .and_then(|value| value.strip_suffix(':'))
        .expect("run output did not contain a session ID");

    for (operation, arguments) in [
        ("status", vec!["status", "--format", "json"]),
        ("sessions", vec!["sessions", "--format", "json"]),
        (
            "deleted-sessions",
            vec!["deleted-sessions", "--format", "json"],
        ),
        ("show", vec!["show", session, "--format", "json"]),
    ] {
        let output = fence(root.path(), &arguments);
        assert_success(operation, &output);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_envelope(&value, operation, "ok");
    }

    let diff = fence(root.path(), &["diff", session, "--format", "json"]);
    assert_eq!(
        diff.status.code(),
        Some(1),
        "diff returned an unexpected status\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&diff.stdout),
        String::from_utf8_lossy(&diff.stderr)
    );
    let diff_json: serde_json::Value = serde_json::from_slice(&diff.stdout).unwrap();
    assert_envelope(&diff_json, "diff", "differences");
    assert_eq!(diff_json["data"]["changes"][0]["status"], "modified");

    let rollback = fence(root.path(), &["rollback", session, "--format", "json"]);
    assert_eq!(rollback.status.code(), Some(3));
    let rollback_json: serde_json::Value = serde_json::from_slice(&rollback.stdout).unwrap();
    assert_envelope(&rollback_json, "rollback", "preview");

    let restore_index = fence(
        root.path(),
        &["restore-index", session, "--yes", "--format", "json"],
    );
    assert_success("restore-index", &restore_index);
    let restore_index_json: serde_json::Value =
        serde_json::from_slice(&restore_index.stdout).unwrap();
    assert_eq!(restore_index_json["schema"], 1);
    assert_eq!(restore_index_json["operation"], "restore-index");
    assert!(matches!(
        restore_index_json["status"].as_str(),
        Some("applied" | "no-change")
    ));

    let restore = fence(
        root.path(),
        &[
            "restore", session, "--file", "file", "--yes", "--format", "json",
        ],
    );
    assert_success("restore", &restore);
    let restore_json: serde_json::Value = serde_json::from_slice(&restore.stdout).unwrap();
    assert_envelope(&restore_json, "restore-file", "applied");
    assert_eq!(fs::read(root.path().join("file")).unwrap(), b"base");

    let doctor = fence(root.path(), &["doctor", "--format", "json"]);
    assert_success("doctor", &doctor);
    let doctor_json: serde_json::Value = serde_json::from_slice(&doctor.stdout).unwrap();
    assert_envelope(&doctor_json, "doctor", "ok");
    assert_eq!(doctor_json["data"]["unfinished_transactions"], 0);

    let gc = fence(root.path(), &["gc", "--dry-run", "--format", "json"]);
    assert_success("gc", &gc);
    let gc_json: serde_json::Value = serde_json::from_slice(&gc.stdout).unwrap();
    assert_envelope(&gc_json, "gc", "ok");

    for (operation, arguments) in [
        ("recover", vec!["recover", "--format", "json"]),
        (
            "recover-transactions",
            vec!["recover-transactions", "--yes", "--format", "json"],
        ),
    ] {
        let output = fence(root.path(), &arguments);
        assert_success(operation, &output);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_envelope(&value, operation, "ok");
    }
}

fn assert_envelope(value: &serde_json::Value, operation: &str, status: &str) {
    assert_eq!(value["schema"], 1);
    assert_eq!(value["operation"], operation);
    assert_eq!(value["status"], status);
    assert!(value.get("data").is_some());
}

fn fence(root: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fence"))
        .args(arguments)
        .current_dir(root)
        .env("PATH", "/fence-test-path-without-git")
        .env(
            "FENCE_CONFIG_FILE",
            root.join("deliberately-absent-config.toml"),
        )
        .output()
        .unwrap()
}

fn assert_success(operation: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{operation} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn repository() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let git = root.path().join(".git");
    fs::create_dir_all(git.join("objects")).unwrap();
    fs::create_dir_all(git.join("refs").join("heads")).unwrap();
    fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
    fs::write(
        git.join("config"),
        b"[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
    )
    .unwrap();
    root
}
