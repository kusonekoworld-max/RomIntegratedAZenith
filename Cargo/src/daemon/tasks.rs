// ---------------- background tasks started over AIDL ----------------
// The GameSpace app is not root, so anything that needs root (bypass charge
// diagnosis, ...) runs here, inside the daemon. Only the names in ALLOWED can
// be started and the argument is restricted to a node name, so the binder
// interface is not a generic command runner.
//
// A task writes its output to `API_DIR/task_<name>.out` as it goes and drops
// `API_DIR/task_<name>.done` (the exit code) when it finishes. The app tails it
// through `getTaskOutput`, which also appends a `##DONE <code>` line.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::thread;

use crate::common::log::{comp, info, warn};
use crate::common::paths::API_DIR;

const ALLOWED: &[&str] = &["bypass_list", "bypass_check", "bypass_test", "bypass_restore", "mali_check"];

static RUNNING: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn out_path(name: &str) -> String {
    format!("{API_DIR}/task_{name}.out")
}

fn done_path(name: &str) -> String {
    format!("{API_DIR}/task_{name}.done")
}

fn arg_ok(arg: &str) -> bool {
    arg.len() <= 64 && arg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn run_one(name: &str, arg: &str, out: &mut dyn Write) -> i32 {
    match name {
        "mali_check" => {
            let has = glob::glob("/sys/class/devfreq/*.mali")
                .map(|mut p| p.next().is_some())
                .unwrap_or(false);
            let _ = writeln!(out, "{}", if has { "true" } else { "false" });
            if has { 0 } else { 1 }
        }
        _ => crate::cmd::bypass::run_task(name, arg, out),
    }
}

/// Start `name` in the background. `false` when it is not allowed, the
/// argument is not acceptable, or the same task is still running.
pub(crate) fn start(name: &str, arg: &str) -> bool {
    if !ALLOWED.contains(&name) || !arg_ok(arg) {
        warn!(comp::TASKS, "rejected task '{name}' arg '{arg}'");
        return false;
    }
    {
        let mut r = RUNNING.lock().unwrap_or_else(|e| e.into_inner());
        if r.iter().any(|n| n == name) {
            return false;
        }
        r.push(name.to_string());
    }
    let _ = fs::remove_file(done_path(name));
    let file = fs::File::create(out_path(name));
    let _ = fs::set_permissions(out_path(name), fs::Permissions::from_mode(0o644));
    let (name, arg) = (name.to_string(), arg.to_string());
    thread::spawn(move || {
        info!(comp::TASKS, "task {name} started");
        let code = match file {
            Ok(mut f) => run_one(&name, &arg, &mut f),
            Err(e) => {
                warn!(comp::TASKS, "task {name}: cannot open output: {e}");
                -1
            }
        };
        let _ = fs::write(done_path(&name), format!("{code}\n"));
        RUNNING.lock().unwrap_or_else(|e| e.into_inner()).retain(|n| n != &name);
        info!(comp::TASKS, "task {name} finished code={code}");
    });
    true
}

/// Output so far, plus a `##DONE <code>` line once the task finished.
pub(crate) fn output(name: &str) -> String {
    if !ALLOWED.contains(&name) {
        return String::new();
    }
    let mut text = fs::read_to_string(out_path(name)).unwrap_or_default();
    if let Ok(code) = fs::read_to_string(done_path(name)) {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&format!("##DONE {}\n", code.trim()));
    }
    text
}
