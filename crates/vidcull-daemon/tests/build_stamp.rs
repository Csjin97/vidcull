use std::process::Command;

#[test]
fn build_stamp_flag_prints_stamp_and_exits_zero() {
    let output = Command::new(env!("CARGO_BIN_EXE_vidcull-daemon"))
        .arg("--build-stamp")
        .output()
        .expect("spawn vidcull-daemon --build-stamp");

    assert!(
        output.status.success(),
        "expected exit 0, got {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf8");
    assert_eq!(stdout.trim(), vidcull_daemon::build_stamp());
}
