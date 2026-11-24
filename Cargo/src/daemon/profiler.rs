// ---------------- profiler (profile switching + per-game overrides) ----------------
use std::collections::BTreeSet;
use std::ffi::c_char;
use std::fs;
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use crate::common::log::{comp, debug, info};
use crate::common::android_ffi::prop_get;
use crate::common::prop;
use crate::common::shell;
use crate::daemon::applist;
use crate::daemon::state::{saver, screen, write_atomic};

pub(crate) use crate::common::paths::{CURRENT_PROFILE as PROFILE_FILE, GAMEINFO as GAMEINFO_FILE};

pub(crate) const MODE_PERF: u8 = 1;
pub(crate) const MODE_BALANCED: u8 = 2;
pub(crate) const MODE_ECO: u8 = 3;

const SCREEN_OFF_GRACE: Duration = Duration::from_secs(10);

static CUR: Mutex<u8> = Mutex::new(0);
static GRACE: Mutex<Option<Instant>> = Mutex::new(None);
static TX: Mutex<Option<mpsc::Sender<u8>>> = Mutex::new(None);

#[derive(Default)]
struct ActiveOverrides {
    pkg: Option<String>,
    dnd_enabled: bool,
    saved_litemode: Option<String>,
    saved_min_rr: Option<String>,
    saved_peak_rr: Option<String>,
    downscale_pkg: Option<String>,
}

static OVERRIDES: Mutex<ActiveOverrides> = Mutex::new(ActiveOverrides {
    pkg: None,
    dnd_enabled: false,
    saved_litemode: None,
    saved_min_rr: None,
    saved_peak_rr: None,
    downscale_pkg: None,
});

fn disable_tweak() -> bool {
    prop::getprop("persist.sys.azenithIntg.disabletweak") == "1"
}

/// Profile currently applied (0 = nothing yet).
pub(crate) fn cur() -> u8 {
    *CUR.lock().unwrap_or_else(|e| e.into_inner())
}

/// Manual mode = `AIenabled == "0"` (same meaning as AZenith main). In auto
/// mode (default, "1") a leftover `manual_profile` is ignored.
fn manual_mode() -> bool {
    prop_get("persist.sys.azenithConfIntg.AIenabled").as_deref() == Some("0")
}

fn manual_profile_override() -> Option<u8> {
    if !manual_mode() {
        return None;
    }
    match prop::getprop("persist.sys.azenithConfIntg.manual_profile").trim() {
        "1" => Some(MODE_PERF),
        "2" => Some(MODE_BALANCED),
        "3" => Some(MODE_ECO),
        _ => None,
    }
}

pub(crate) fn init() {
    info!(comp::PROFILER, "profiler initialized");
    let (tx, rx) = mpsc::channel::<u8>();
    *TX.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    thread::spawn(move || {
        let mut last = 0u8;
        while let Ok(mode) = rx.recv() {
            if mode == last { continue; }
            last = mode;
            run_profile(mode);
        }
    });
}

fn queue_profile(mode: u8) {
    let tx = TX.lock().unwrap_or_else(|e| e.into_inner()).as_ref().cloned();
    match tx {
        Some(t) => { let _ = t.send(mode); }
        None => run_profile(mode),
    }
}

pub(crate) fn mode_name(m: u8) -> &'static str {
    match m {
        0 => "none",
        MODE_PERF => "performance",
        MODE_BALANCED => "balanced",
        MODE_ECO => "eco",
        _ => "unknown",
    }
}

pub(crate) fn cur_name() -> &'static str {
    mode_name(*CUR.lock().unwrap_or_else(|e| e.into_inner()))
}

fn time_str() -> String {
    unsafe {
        let mut t: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut t);
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t.tv_sec, &mut tm);
        let mut buf = [0u8; 32];
        let n = libc::strftime(buf.as_mut_ptr() as *mut c_char, buf.len(), c"%H:%M:%S".as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

fn uid_of(pid: i32) -> i32 {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("Uid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

fn write_gameinfo(active: &Option<(String, BTreeSet<i32>)>) {
    let t = time_str();
    let body = match active {
        Some((pkg, pids)) => {
            let pid = pids.iter().next().copied().unwrap_or(0);
            format!("{pkg} {pid} {}\nTime: {t}\n", uid_of(pid))
        }
        None => format!("NULL 0 0\nTime: {t}\n"),
    };
    write_atomic(GAMEINFO_FILE, &body);
}

fn run_profile(mode: u8) {
    if disable_tweak() {
        info!(comp::PROFILER, "disabletweak=1, skipping profile apply mode={}", mode_name(mode));
        return;
    }
    let t = Instant::now();
    crate::cmd::settings::apply_mode(mode);
    info!(comp::PROFILER, "profile apply finished mode={} in {:.0?}", mode_name(mode), t.elapsed());
}

pub(crate) fn run_initialize() {
    run_profile(0);
}

fn is_truthy(val: &str) -> bool {
    matches!(val.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "enabled" | "yes")
}

fn clear_game_overrides() {
    let mut ov = OVERRIDES.lock().unwrap_or_else(|e| e.into_inner());
    if ov.pkg.is_none() {
        return;
    }
    let prev_pkg = ov.pkg.take().unwrap_or_default();
    let restore_dnd = ov.dnd_enabled;
    ov.dnd_enabled = false;
    let restore_lite = ov.saved_litemode.take();
    let restore_min_rr = ov.saved_min_rr.take();
    let restore_peak_rr = ov.saved_peak_rr.take();
    let restore_downscale = ov.downscale_pkg.take();
    drop(ov);

    info!(comp::PROFILER, "restoring per-game overrides for {prev_pkg}");
    if let Some(prev) = restore_lite {
        prop::setprop("persist.sys.azenithConfIntg.litemode", &prev);
    }

    thread::spawn(move || {
        if restore_dnd {
            let _ = shell::system(comp::PROFILER, "cmd notification set_dnd off");
        }
        if let Some(pkg) = restore_downscale {
            let _ = shell::system(comp::PROFILER, &format!("cmd game downscale disable {pkg}"));
        }
        if let Some(min_rr) = restore_min_rr {
            if min_rr.is_empty() || min_rr == "null" {
                let _ = shell::system(comp::PROFILER, "settings delete system min_refresh_rate");
            } else {
                let _ = shell::system(comp::PROFILER, &format!("settings put system min_refresh_rate {min_rr}"));
            }
        }
        if let Some(peak_rr) = restore_peak_rr {
            if peak_rr.is_empty() || peak_rr == "null" {
                let _ = shell::system(comp::PROFILER, "settings delete system peak_refresh_rate");
            } else {
                let _ = shell::system(comp::PROFILER, &format!("settings put system peak_refresh_rate {peak_rr}"));
            }
        }
    });
}

fn apply_game_overrides(pkg: &str, pids: &BTreeSet<i32>) {
    if disable_tweak() {
        return;
    }
    let Some(cfg) = applist::get_game_config(pkg) else {
        return;
    };

    {
        let ov = OVERRIDES.lock().unwrap_or_else(|e| e.into_inner());
        if ov.pkg.as_deref() == Some(pkg) {
            return;
        }
    }
    clear_game_overrides();

    let mut ov = OVERRIDES.lock().unwrap_or_else(|e| e.into_inner());
    ov.pkg = Some(pkg.to_string());

    // Per-game Lite Mode override
    if let Some(v) = cfg.get("perf_lite_mode") {
        if v != "default" {
            ov.saved_litemode = Some(prop::getprop("persist.sys.azenithConfIntg.litemode"));
            let lite_val = if is_truthy(v) { "1" } else { "0" };
            prop::setprop("persist.sys.azenithConfIntg.litemode", lite_val);
        }
    }

    // Per-game renderer hint
    if let Some(r) = cfg.get("renderer") {
        match r.trim().to_ascii_lowercase().as_str() {
            "skiagl" | "opengl" => prop::setprop("debug.hwui.renderer", "skiagl"),
            "skiavk" | "vulkan" => prop::setprop("debug.hwui.renderer", "skiavk"),
            _ => {}
        }
    }

    let enable_dnd = cfg.get("dnd_on_gaming").map(|v| is_truthy(v)).unwrap_or(false);
    if enable_dnd {
        ov.dnd_enabled = true;
    }

    let rr_val = cfg
        .get("refresh_rate")
        .map(|s| s.trim().trim_end_matches("Hz").trim_end_matches("hz").trim().to_string())
        .filter(|s| !s.is_empty() && s != "default" && s.chars().all(|c| c.is_ascii_digit()));

    let downscale_val = cfg
        .get("resolution_downscale")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "default" && s.chars().all(|c| c.is_ascii_digit() || c == '.'));
    if downscale_val.is_some() {
        ov.downscale_pkg = Some(pkg.to_string());
    }

    let boost_prio = cfg.get("app_priority").map(|v| is_truthy(v)).unwrap_or(false);
    let pids_copy: Vec<i32> = pids.iter().copied().collect();
    let pkg_owned = pkg.to_string();
    drop(ov);

    info!(comp::PROFILER, "applying per-game overrides for {pkg_owned}");
    thread::spawn(move || {
        if enable_dnd {
            let _ = shell::system(comp::PROFILER, "cmd notification set_dnd priority");
        }
        if boost_prio {
            for pid in pids_copy {
                if pid > 0 {
                    let _ = shell::system(comp::PROFILER, &format!("renice -n -10 -p {pid}"));
                }
            }
        }
        if let Some(scale) = downscale_val {
            let _ = shell::system(comp::PROFILER, &format!("cmd game downscale {scale} {pkg_owned}"));
        }
        if let Some(hz) = rr_val {
            let _ = shell::system(comp::PROFILER, &format!("settings put system min_refresh_rate {hz}"));
            let _ = shell::system(comp::PROFILER, &format!("settings put system peak_refresh_rate {hz}"));
        }
    });
}

fn apply(mode: u8, active: &Option<(String, BTreeSet<i32>)>, why: &str) {
    let from = {
        let mut c = CUR.lock().unwrap_or_else(|e| e.into_inner());
        let f = *c;
        *c = mode;
        f
    };
    info!(comp::PROFILER, "profile: [{}] -> [{}] ({why})", mode_name(from), mode_name(mode));
    write_gameinfo(active);
    write_atomic(PROFILE_FILE, &format!("{mode}\n"));

    if mode == MODE_PERF {
        if let Some((pkg, pids)) = active {
            apply_game_overrides(pkg, pids);
        }
    } else {
        clear_game_overrides();
    }

    queue_profile(mode);
    broadcast_profile_change(mode, active);
    // bypass charging only runs in performance
    crate::cmd::bypass::reevaluate_async();
}

fn broadcast_profile_change(mode: u8, active: &Option<(String, BTreeSet<i32>)>) {
    let mode_str = mode_name(mode);
    let pkg_arg = match active {
        Some((pkg, _)) if !pkg.is_empty() && pkg != "NULL" => format!(" --es pkg {pkg}"),
        _ => String::new(),
    };
    let cmd = format!(
        "am broadcast --user 0 -a com.rianixia.settings.SHOW_NOTIFICATION -n com.rianixia.settings.overlay/com.rianixia.settings.overlay.OverlayService --es category azenith --es profile {mode_str}{pkg_arg} &"
    );
    thread::spawn(move || {
        let _ = shell::system(comp::PROFILER, &cmd);
    });
}

pub(crate) fn refresh_now(why: &str) {
    let mut g = crate::daemon::state::st();
    crate::daemon::state::apply(&mut *g, why);
}

pub(crate) fn checkup(active: &Option<(String, BTreeSet<i32>)>, why: &str) {
    let cur = *CUR.lock().unwrap_or_else(|e| e.into_inner());
    let screen_off = screen() == "off";
    let saver_on = saver() == "on";

    let want = if let Some(forced) = manual_profile_override() {
        forced
    } else if active.is_some() && !screen_off {
        MODE_PERF
    } else if saver_on {
        MODE_ECO
    } else {
        MODE_BALANCED
    };

    let mut grace = GRACE.lock().unwrap_or_else(|e| e.into_inner());
    if manual_profile_override().is_none()
        && cur == MODE_PERF
        && want != MODE_PERF
        && active.is_some()
        && screen_off
    {
        match *grace {
            None => {
                *grace = Some(Instant::now());
                info!(comp::PROFILER, "screen off grace period started (timeout=10s, keep performance)");
            }
            Some(t) => debug!(comp::PROFILER, "grace period active {:.0}s ({why})", t.elapsed().as_secs_f32()),
        }
        return;
    }
    *grace = None;

    if want == cur {
        if mode_name(want) == "performance" {
            if let Some((pkg, pids)) = active {
                apply_game_overrides(pkg, pids);
            }
        }
        debug!(comp::PROFILER, "profile unchanged [{}] ({why})", mode_name(want));
        return;
    }
    apply(want, active, why);
}

pub(crate) fn tick() {
    let expired = {
        let g = GRACE.lock().unwrap_or_else(|e| e.into_inner());
        matches!(*g, Some(t) if t.elapsed() >= SCREEN_OFF_GRACE)
    };
    if !expired { return; }
    info!(comp::PROFILER, "grace period expired, dropping performance");
    *GRACE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    refresh_now("grace period expired");
}
