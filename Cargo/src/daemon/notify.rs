// App notifications: notification / toast / sidepanel. Same broadcasts as the
// module's archdaemon, sent to the AZenith app's ZenithReceiver. The daemon
// already runs as root, so `am` is forked directly (no `su -c`).

use std::sync::Mutex;
use std::thread;

use crate::common::log::{comp, info, warn};
use crate::common::prop;
use crate::common::shell;

const RECEIVER: &str = "zx.azenith/zx.azenith.receiver.ZenithReceiver";
const ACTION_MANAGE: &str = "zx.azenith.ACTION_MANAGE";
const ACTION_SHOW_SIDEPANEL: &str = "zx.azenith.ACTION_SHOW_SIDEPANEL";
const PROP_TOAST: &str = "persist.sys.azenithConfIntg.showtoast";

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn broadcast(action: &str, extras: String) {
    let cmd = format!("am broadcast --user 0 -a {action} -n {RECEIVER} {extras} >/dev/null 2>&1");
    thread::spawn(move || {
        let rc = shell::system(comp::NOTIFY, &cmd);
        if rc != 0 {
            warn!(comp::NOTIFY, "broadcast failed exit={rc}");
        }
    });
}

pub(crate) fn notify(title: &str, text: &str, chrono: bool, timeout_ms: i32) {
    let mut extras = format!(
        "--es notifytitle {} --es notifytext {} --ez chrono_bool {}",
        quote(title),
        quote(text),
        if chrono { "true" } else { "false" }
    );
    if timeout_ms > 0 {
        extras.push_str(&format!(" --es timeout '{timeout_ms}'"));
    }
    broadcast(ACTION_MANAGE, extras);
}

pub(crate) fn toast(message: &str) {
    if prop::getprop(PROP_TOAST) != "1" {
        return;
    }
    broadcast(ACTION_MANAGE, format!("--es toasttext {}", quote(message)));
}

pub(crate) fn clear_all() {
    broadcast(ACTION_MANAGE, "--ez clearall true".to_string());
}

pub(crate) fn show_sidepanel(app: &str) {
    info!(comp::NOTIFY, "sidepanel broadcast action=show app={app}");
    broadcast(ACTION_SHOW_SIDEPANEL, format!("--es sidepanel_pkg {}", quote(app)));
}

static SIDEPANEL_PKG: Mutex<Option<String>> = Mutex::new(None);

/// Manual mode never applies a profile on game start, so show the sidepanel
/// directly, once per game session (same as the module).
pub(crate) fn sync_manual_sidepanel(active_pkg: Option<&str>) {
    let mut last = SIDEPANEL_PKG.lock().unwrap_or_else(|e| e.into_inner());
    match active_pkg {
        Some(pkg) if !pkg.is_empty() && pkg != "NULL" => {
            if last.as_deref() != Some(pkg) {
                *last = Some(pkg.to_string());
                show_sidepanel(pkg);
            }
        }
        _ => *last = None,
    }
}
