//! install.sh on macOS never replaces a reason-network signed by a developer team with a build that team didn't sign:
//! macOS would take it for a new program and ask again whether it may reach the local network. Run with stand-ins
//! for `uname` (a Mac) and `codesign` (which reads a `# team:` line from the file), and a local release.
#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

fn script(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A reason-network stand-in at `version`, signed by `team` (None: ad hoc).
fn engine(path: &Path, version: &str, team: Option<&str>) {
    let team = team.map(|t| format!("# team: {t}\n")).unwrap_or_default();
    script(path, &format!("{team}echo 'reason-network {version}'"));
}

/// Installs a release signed by `new` over one signed by `old`; returns whether it succeeded and the version left.
fn update(name: &str, old: Option<&str>, new: Option<&str>) -> (bool, String) {
    let root = std::env::temp_dir().join(format!("rn-install-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let (fake, release, bin) = (root.join("fake"), root.join("release"), root.join("bin"));
    for d in [&fake, &release, &bin] {
        fs::create_dir_all(d).unwrap();
    }
    script(&fake.join("uname"), r#"case "$1" in -s) echo Darwin ;; -m) echo arm64 ;; esac"#);
    script(
        &fake.join("codesign"),
        r#"for f; do :; done; t="$(sed -n 's/^# team: //p' "$f")"; echo "TeamIdentifier=${t:-not set}" >&2"#,
    );
    engine(&bin.join("reason-network"), "1.0.0", old);
    engine(&release.join("reason-network"), "1.0.1", new);
    let archive = "reason-network-aarch64-apple-darwin.tar.gz";
    let pack = format!(
        "tar -czf {archive} reason-network && rm reason-network && \
         {{ shasum -a 256 {archive} 2>/dev/null || sha256sum {archive}; }} > {archive}.sha256"
    );
    assert!(Command::new("sh").arg("-c").arg(pack).current_dir(&release).status().unwrap().success());

    let path = format!("{}:{}", fake.display(), std::env::var("PATH").unwrap());
    let out = Command::new("sh")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh"))
        .env("PATH", path)
        .env("HOME", &root)
        .env("REASON_NETWORK_BIN_DIR", &bin)
        .env("REASON_NETWORK_DOWNLOAD_BASE", format!("file://{}", release.display()))
        .env("REASON_NETWORK_NO_PROMPT", "1")
        .output()
        .unwrap();
    let left = Command::new(bin.join("reason-network")).output().unwrap();
    let _ = fs::remove_dir_all(&root);
    (out.status.success(), String::from_utf8_lossy(&left.stdout).trim().to_string())
}

#[test]
fn a_signed_install_takes_only_its_teams_builds() {
    assert_eq!(update("same-team", Some("TEAM1"), Some("TEAM1")), (true, "reason-network 1.0.1".into()));
    assert_eq!(update("unsigned", Some("TEAM1"), None), (false, "reason-network 1.0.0".into()));
    assert_eq!(update("other-team", Some("TEAM1"), Some("TEAM2")), (false, "reason-network 1.0.0".into()));
}

#[test]
fn an_unsigned_install_takes_any_build() {
    assert_eq!(update("was-unsigned", None, None), (true, "reason-network 1.0.1".into()));
    assert_eq!(update("now-signed", None, Some("TEAM1")), (true, "reason-network 1.0.1".into()));
}
