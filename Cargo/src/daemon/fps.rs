// ---------------- fps source: presented frame rate of the focused task ----------------
// Reduced port of `sysdaemon/src/fps.rs` (AZenith main): only the
// `windowmanager` source. The daemon is uid 0, which AMS lets register
// WindowManager.registerTaskFpsCallback (ACCESS_FPS_COUNTER), so it simply runs
// `zx.binder.FpsClient` (inside the embedded resolver APK) under app_process
// while a game is on screen and stops it when the game or the screen goes away.
//
// Output: `API_DIR/current_fps` = "<fps> <epoch_ms>" (read by the overlay and by
// the recorder). While no game runs the file holds NO_ACTIVE_GAME.
// No frame-time histogram is produced (that needs the uprobe / SurfaceFlinger
// sources of main), so low1 / low01 / jank are not available.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::common::log::{comp, info, warn};
use crate::common::paths::{API_DIR, BASE_DIR};

pub(crate) const NO_GAME_MARKER: &str = "NO_ACTIVE_GAME";
const RETRY_DELAY: Duration = Duration::from_secs(5);
const CHECK_EVERY: Duration = Duration::from_secs(3);

static WANT: AtomicBool = AtomicBool::new(false);
static WAKE: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

pub(crate) fn fps_file() -> String {
    format!("{API_DIR}/current_fps")
}

fn apk_path() -> String {
    format!("{BASE_DIR}/fpsclient.apk")
}

fn log_path() -> String {
    format!("{BASE_DIR}/fpsclient.log")
}

fn wake() {
    let (m, c) = &WAKE;
    *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
    c.notify_all();
}

/// Called from the state machine: `true` while a game is active and the screen is on.
/// Cheap (an atomic) so it can sit on the state-lock path.
pub(crate) fn set_wanted(want: bool) {
    if WANT.swap(want, Ordering::Relaxed) != want {
        wake();
    }
}

fn stage_apk() -> bool {
    let bytes = crate::daemon::codes::resolver_apk();
    let path = apk_path();
    if fs::metadata(&path).map(|m| m.len() == bytes.len() as u64).unwrap_or(false) {
        return true;
    }
    match fs::write(&path, bytes) {
        Ok(()) => {
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
            true
        }
        Err(e) => {
            warn!(comp::DAEMON, "fps: cannot stage {path}: {e}");
            false
        }
    }
}

fn spawn_client() -> Option<Child> {
    let apk = apk_path();
    let log = fs::File::create(log_path()).ok();
    let (out, err) = match log {
        Some(f) => match f.try_clone() {
            Ok(f2) => (Stdio::from(f), Stdio::from(f2)),
            Err(_) => (Stdio::null(), Stdio::null()),
        },
        None => (Stdio::null(), Stdio::null()),
    };
    let child = Command::new("/system/bin/app_process")
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .env("ANDROID_ROOT", "/system")
        .env("ANDROID_DATA", "/data")
        .env("ANDROID_RUNTIME_ROOT", "/apex/com.android.runtime")
        .env("ANDROID_ART_ROOT", "/apex/com.android.art")
        .env("ANDROID_I18N_ROOT", "/apex/com.android.i18n")
        .env("ANDROID_TZDATA_ROOT", "/apex/com.android.tzdata")
        .arg(format!("-Djava.class.path={apk}"))
        .arg("/")
        .arg("zx.binder.FpsClient")
        .arg(format!("--out={}", fps_file()))
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .spawn();
    match child {
        Ok(c) => {
            info!(comp::DAEMON, "fps: client started (pid {})", c.id());
            Some(c)
        }
        Err(e) => {
            warn!(comp::DAEMON, "fps: failed to spawn app_process: {e}");
            None
        }
    }
}

fn write_marker() {
    let path = fps_file();
    if fs::read_to_string(&path).map(|s| s.trim() == NO_GAME_MARKER).unwrap_or(false) {
        return;
    }
    if fs::write(&path, format!("{NO_GAME_MARKER}\n")).is_ok() {
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
    }
}

fn worker() {
    let mut child: Option<Child> = None;
    let mut last_spawn: Option<Instant> = None;
    loop {
        let want = WANT.load(Ordering::Relaxed);

        if let Some(c) = child.as_mut() {
            if let Ok(Some(status)) = c.try_wait() {
                warn!(comp::DAEMON, "fps: client exited ({status}), see {}", log_path());
                child = None;
            }
        }

        if want {
            if child.is_none() && last_spawn.map(|t| t.elapsed() >= RETRY_DELAY).unwrap_or(true) && stage_apk() {
                child = spawn_client();
                last_spawn = Some(Instant::now());
            }
        } else {
            if let Some(mut c) = child.take() {
                let _ = c.kill();
                let _ = c.wait();
                info!(comp::DAEMON, "fps: client stopped");
            }
            write_marker();
        }

        let (m, c) = &WAKE;
        let mut woken = m.lock().unwrap_or_else(|e| e.into_inner());
        if !*woken {
            let (g, _) = c.wait_timeout(woken, CHECK_EVERY).unwrap_or_else(|e| e.into_inner());
            woken = g;
        }
        *woken = false;
    }
}

pub(crate) fn start() {
    let _ = fs::remove_file(log_path());
    thread::spawn(worker);
}
