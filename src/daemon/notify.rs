//! Desktop notifications, for a message that needs the person and not only their agent (`notify`). Shown with
//! what the system has: osascript on macOS, notify-send on Linux, a toast from Windows PowerShell. Where there is
//! nothing to show one with, the sender is told; people turn them off in their system's notification settings.

use super::{Daemon, log};
use crate::{agent, proto::Message, util};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// At most one notification per person sending in this time: a looping agent can't flood the screen.
const GAP: Duration = Duration::from_secs(60);
/// Older than this (it waited in an outbox while this machine was off), it no longer pops up.
const STALE_MS: i64 = 15 * 60 * 1000;

impl Daemon {
    /// Shows `msg` to this machine's person. Returns whether it was handed to the system's notifications (which may
    /// still hide it, say if the person turned them off), and a note for the sender.
    pub(super) fn notify(&self, msg: &Message) -> (bool, String) {
        if util::now_ms() - msg.sent_at > STALE_MS {
            return (false, "not popped up: it's over 15 minutes old".into());
        }
        let person = msg.from.split('/').next().unwrap_or_default().to_string();
        let title = match msg.to.split_once('/') {
            Some((_, session)) => format!("{} → {session}", msg.from),
            None => msg.from.clone(),
        };
        let mut shown = self.shown.lock().unwrap();
        shown.retain(|_, at| at.elapsed() < GAP);
        if shown.contains_key(&person) {
            return (false, format!("not popped up: {} had one from {person} a moment ago", self.machine));
        }
        let Some(mut cmd) = command(&title, &util::one_line(&msg.body, 200)) else {
            return (false, format!("not popped up: {} has no desktop to show it on", self.machine));
        };
        shown.insert(person, Instant::now());
        std::thread::spawn(move || {
            match cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).output() {
                Ok(o) if o.status.success() => {}
                Ok(o) => log(&format!("notification failed: {}", String::from_utf8_lossy(&o.stderr).trim())),
                Err(e) => log(&format!("notification failed: {e}")),
            }
        });
        (true, format!("popped up for {} ({})", self.person, self.machine))
    }
}

#[cfg(target_os = "macos")]
fn command(title: &str, body: &str) -> Option<Command> {
    let mut cmd = Command::new(agent::find_bin("osascript")?);
    let show = r#"display notification (item 2 of argv) with title (item 1 of argv) sound name "Glass""#;
    cmd.args(["-e", "on run argv", "-e", show, "-e", "end run", title, body]);
    Some(cmd)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command(title: &str, body: &str) -> Option<Command> {
    let mut cmd = notify_send()?;
    // `--` keeps a body starting with `-` from being taken for an option.
    cmd.args(["--app-name=Reason Network".into(), "--".into(), markup(title), markup(body)]);
    Some(cmd)
}

/// A daemon started outside the desktop session may lack the session bus's address; without a bus, no desktop.
#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn notify_send() -> Option<Command> {
    notify_send_at(agent::find_bin("notify-send")?)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn notify_send_at(bin: std::path::PathBuf) -> Option<Command> {
    let mut cmd = Command::new(bin);
    let bus = match std::env::var_os("DBUS_SESSION_BUS_ADDRESS") {
        Some(bus) => bus,
        None => {
            let bus = format!("/run/user/{}/bus", unsafe { libc::getuid() });
            if !std::path::Path::new(&bus).exists() {
                return None;
            }
            format!("unix:path={bus}").into()
        }
    };
    // Linger keeps a user bus up with nobody at the screen (a server, a box reached over SSH): only a notification
    // server on it makes a desktop. One started on demand counts once it runs.
    if !notifications_on(&bus) {
        return None;
    }
    cmd.env("DBUS_SESSION_BUS_ADDRESS", bus);
    Some(cmd)
}

/// Whether the bus at `bus` has a notification server. Only a clear no counts: without busctl, or on a bus it can't
/// reach, notify-send finds out. It's asked under the daemon's lock, so a bus that hangs gets a second in all
/// (`--timeout` doesn't cover connecting).
#[cfg(all(unix, not(target_os = "macos")))]
fn notifications_on(bus: &std::ffi::OsStr) -> bool {
    let Some(busctl) = super::ask::asker_bin("busctl", &["/usr/bin/busctl", "/run/current-system/sw/bin/busctl"])
    else {
        return true;
    };
    let Ok(mut child) = Command::new(busctl)
        .env("DBUS_SESSION_BUS_ADDRESS", bus)
        .args(["--user", "--timeout=1", "call", "org.freedesktop.DBus", "/org/freedesktop/DBus"])
        .args(["org.freedesktop.DBus", "NameHasOwner", "s", "org.freedesktop.Notifications"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return true;
    };
    let until = Instant::now() + Duration::from_secs(1);
    while matches!(child.try_wait(), Ok(None)) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
    }
    let _ = child.kill();
    !matches!(child.wait_with_output(), Ok(o) if o.status.success() && o.stdout.trim_ascii() == b"b false")
}

/// Most notification servers read markup.
#[cfg(all(unix, not(target_os = "macos")))]
pub(super) fn markup(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(windows)]
fn command(title: &str, body: &str) -> Option<Command> {
    const TOAST: &str = "$ErrorActionPreference = 'Stop'
$n = [Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime]
$x = $n::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$t = $x.GetElementsByTagName('text')
$t.Item(0).AppendChild($x.CreateTextNode($env:TINCAN_TITLE)) > $null
$t.Item(1).AppendChild($x.CreateTextNode($env:TINCAN_BODY)) > $null
$app = '{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe'
$n::CreateToastNotifier($app).Show([Windows.UI.Notifications.ToastNotification]::new($x))";
    let mut cmd = powershell(TOAST)?;
    cmd.env("TINCAN_TITLE", title).env("TINCAN_BODY", body);
    Some(cmd)
}

#[cfg(windows)]
pub(super) fn powershell(script: &str) -> Option<Command> {
    Some(powershell_at(agent::find_bin("powershell")?, script))
}

#[cfg(windows)]
pub(super) fn powershell_at(bin: std::path::PathBuf, script: &str) -> Command {
    use std::os::windows::process::CommandExt;
    let mut cmd = Command::new(bin);
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    // CREATE_NO_WINDOW: no console flashing up next to the notification.
    cmd.creation_flags(0x0800_0000);
    cmd
}

#[cfg(all(test, unix, not(target_os = "macos")))]
mod tests {
    use super::*;
    use std::io::BufRead;

    #[test]
    fn a_bus_without_a_notification_server_is_no_desktop() {
        let Some(daemon) = agent::find_bin("dbus-daemon").filter(|_| agent::find_bin("busctl").is_some()) else {
            return;
        };
        let mut bus = Command::new(daemon)
            .args(["--session", "--nofork", "--print-address"])
            .arg(format!("--address=unix:tmpdir={}", std::env::temp_dir().display()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut address = String::new();
        std::io::BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).unwrap();
        let on = notifications_on(address.trim().as_ref());
        bus.kill().unwrap();
        bus.wait().unwrap();
        assert!(!on);
        // A bus it can't reach is left to notify-send.
        assert!(notifications_on("unix:path=/nonexistent".as_ref()));
    }
}
