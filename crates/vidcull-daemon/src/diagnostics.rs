use std::path::{Path, PathBuf};

const BUNDLE_PREFIXES: [&str; 2] = ["vidcull-daemon", "vidcull-app"];

pub fn collect_diagnostic_bundle(
    logs_dir: &Path,
    dest_dir: &Path,
) -> std::io::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dest_dir)?;
    let mut collected = Vec::new();

    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return Ok(collected);
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !is_allowlisted(name_str) {
            continue;
        }
        if !entry.path().is_file() {
            continue;
        }
        let dst = dest_dir.join(&name);
        if std::fs::copy(entry.path(), &dst).is_ok() {
            collected.push(dst);
        }
    }
    Ok(collected)
}

fn is_allowlisted(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("log"))
        && BUNDLE_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_matches_rolling_logs_only() {
        assert!(is_allowlisted("vidcull-daemon.2026-06-19.log"));
        assert!(is_allowlisted("vidcull-app.2026-06-19.log"));
        assert!(!is_allowlisted("notes.txt"));
        assert!(!is_allowlisted("keymap"));
        assert!(!is_allowlisted("vidcull-daemon.log.bak"));
        assert!(!is_allowlisted("secret.mp4"));
    }

    #[test]
    fn bundle_excludes_keymap_and_plaintext_paths() {
        let root = tempfile::tempdir().expect("tempdir");
        let logs = root.path().join("logs");
        std::fs::create_dir_all(&logs).expect("mk logs");

        std::fs::write(
            logs.join("vidcull-daemon.2026-06-19.log"),
            "INFO file decoded file=abadc0de12345678.mp4 codec=H264\n",
        )
        .expect("write daemon log");
        std::fs::write(
            logs.join("vidcull-app.2026-06-19.log"),
            "WARN sidecar spawn failed\n",
        )
        .expect("write app log");
        std::fs::write(logs.join("notes.txt"), "scratch — do not bundle").expect("write notes");

        let secret_path = "C:/Users/alice/Videos/holiday.mp4";
        std::fs::write(
            root.path().join("keymap"),
            format!("# vidcull keymap v1\nsalt deadbeef\nabadc0de12345678.mp4\t{secret_path}\n"),
        )
        .expect("write keymap");

        let dest = root.path().join("bundle");
        let collected = collect_diagnostic_bundle(&logs, &dest).expect("collect");

        let names: Vec<String> = collected
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(
            names.iter().any(|n| n.starts_with("vidcull-daemon")),
            "daemon log missing: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.starts_with("vidcull-app")),
            "app log missing: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "notes.txt"),
            "non-allowlisted file collected: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "keymap"),
            "keymap was collected: {names:?}"
        );
        assert!(
            !dest.join("keymap").exists(),
            "keymap copied into bundle dir"
        );

        let mut bundle_bytes = Vec::new();
        for p in &collected {
            bundle_bytes.extend_from_slice(&std::fs::read(p).expect("read collected"));
        }
        let bundle = String::from_utf8_lossy(&bundle_bytes);
        assert!(
            !bundle.contains(secret_path),
            "plaintext path leaked into bundle bytes"
        );
        assert!(
            !bundle.contains("keymap"),
            "keymap reference leaked into bundle bytes"
        );
    }
}
