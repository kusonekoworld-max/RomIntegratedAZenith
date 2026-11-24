// ---------------- FPS recorder ----------------
// Reduced port of `archdaemon/src/recorder` (AZenith main).
//
// The overlay writes `start` | `stop` to API/record_cmd; the daemon answers in
// API/record_state (`idle` | `recording <session>.csv`). One row per second goes
// to BASE_DIR/frameratedata/<pkg>_<yyyyMMdd_HHmmss>.csv, the summary to the
// same name with .json. Sessions under 30 s are discarded, the newest 30 kept.
//
// Not available here (needs the frame-time histogram from the FPS source of
// main): ft_avg / ft_max / refresh_hz are -1 and low1 / low01 / jank are null.
// Everything is read directly from sysfs/procfs, no forks.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::common::log::{comp, info, warn};
use crate::common::paths::{API_DIR, BASE_DIR, GAMEINFO};
use crate::daemon::state::write_atomic;

const CSV_HEADER: &str =
    "t,fps,refresh_hz,ft_avg,ft_max,cpu_temp,bat_temp,gpu_freq,gpu_load,cpu_freq_l,cpu_freq_m,cpu_freq_b,cpu_load,ma,mv,bat_lvl";
const MIN_SESSION_SECS: u64 = 30;
const KEEP_SESSIONS: usize = 30;
const DEBOUNCE: Duration = Duration::from_millis(500);
const FPS_STALE_MS: u128 = 3000;

fn dir() -> String {
    format!("{BASE_DIR}/frameratedata")
}
fn cmd_path() -> String {
    format!("{API_DIR}/record_cmd")
}
fn state_path() -> String {
    format!("{API_DIR}/record_state")
}

// ---------------- small readers ----------------

fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn read_i64(path: &str) -> Option<i64> {
    read_trim(path)?.split_whitespace().next()?.parse().ok()
}

fn active_pkg() -> Option<String> {
    let text = fs::read_to_string(GAMEINFO).ok()?;
    let pkg = text.lines().next()?.split_whitespace().next()?.to_string();
    if pkg.is_empty() || pkg == "NULL" { None } else { Some(pkg) }
}

fn epoch_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

fn stamp() -> String {
    unsafe {
        let mut t: libc::timespec = std::mem::zeroed();
        libc::clock_gettime(libc::CLOCK_REALTIME, &mut t);
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t.tv_sec, &mut tm);
        let mut buf = [0u8; 32];
        let n = libc::strftime(buf.as_mut_ptr() as *mut libc::c_char, buf.len(), c"%Y%m%d_%H%M%S".as_ptr(), &tm);
        String::from_utf8_lossy(&buf[..n]).into_owned()
    }
}

fn read_fps() -> i64 {
    let Some(raw) = read_trim(&crate::daemon::fps::fps_file()) else { return -1 };
    let mut it = raw.split_whitespace();
    let (Some(f), Some(ts)) = (it.next(), it.next()) else { return -1 };
    let (Ok(f), Ok(ts)) = (f.parse::<i64>(), ts.parse::<u128>()) else { return -1 };
    if epoch_ms().saturating_sub(ts) > FPS_STALE_MS { -1 } else { f }
}

fn cpu_stat() -> Option<(u64, u64)> {
    let text = fs::read_to_string("/proc/stat").ok()?;
    let line = text.lines().next()?;
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 5 {
        return None;
    }
    Some((v.iter().sum(), v[3] + v[4]))
}

fn cpu_temp_c() -> Option<f64> {
    use std::sync::OnceLock;
    static ZONE: OnceLock<Option<String>> = OnceLock::new();
    let zone = ZONE.get_or_init(|| {
        let mut best: Option<(u8, String)> = None;
        for z in glob::glob("/sys/class/thermal/thermal_zone*").ok()?.flatten() {
            let Some(ty) = read_trim(&format!("{}/type", z.display())) else { continue };
            let ty = ty.to_lowercase();
            let rank = if ty.contains("cpu") { 0 } else if ty.contains("soc") || ty.contains("tsens_tz_sensor0") { 1 } else { continue };
            let p = format!("{}/temp", z.display());
            if best.as_ref().map(|(r, _)| rank < *r).unwrap_or(true) {
                best = Some((rank, p));
            }
        }
        best.map(|(_, p)| p)
    });
    read_i64(zone.as_ref()?).map(|v| v as f64 / 1000.0)
}

fn gpu_freq_khz() -> i64 {
    if let Some(hz) = read_i64("/sys/class/kgsl/kgsl-3d0/devfreq/cur_freq") {
        return hz / 1000;
    }
    for p in glob::glob("/sys/class/devfreq/*/cur_freq").into_iter().flatten().flatten() {
        let s = p.display().to_string().to_lowercase();
        if s.contains("gpu") || s.contains("mali") || s.contains("kgsl") {
            if let Some(hz) = read_i64(&p.display().to_string()) {
                return hz / 1000;
            }
        }
    }
    -1
}

fn gpu_load() -> i64 {
    if let Some(v) = read_i64("/sys/class/kgsl/kgsl-3d0/gpu_busy_percentage") {
        return v.clamp(0, 100);
    }
    if let Some(raw) = read_trim("/sys/class/kgsl/kgsl-3d0/gpubusy") {
        let mut it = raw.split_whitespace().filter_map(|x| x.parse::<f64>().ok());
        if let (Some(busy), Some(total)) = (it.next(), it.next()) {
            if total > 0.0 {
                return ((busy * 100.0 / total) as i64).clamp(0, 100);
            }
        }
    }
    -1
}

/// (little, middle, big) kHz from the cpufreq policies, lowest policy first.
fn cpu_freqs() -> (i64, i64, i64) {
    let mut pols: Vec<(u32, i64)> = Vec::new();
    for p in glob::glob("/sys/devices/system/cpu/cpufreq/policy*").into_iter().flatten().flatten() {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let Ok(n) = name.trim_start_matches("policy").parse::<u32>() else { continue };
        if let Some(f) = read_i64(&format!("{}/scaling_cur_freq", p.display())) {
            pols.push((n, f));
        }
    }
    pols.sort_by_key(|(n, _)| *n);
    match pols.len() {
        0 => (-1, -1, -1),
        1 => (pols[0].1, -1, pols[0].1),
        2 => (pols[0].1, -1, pols[1].1),
        n => (pols[0].1, pols[n / 2].1, pols[n - 1].1),
    }
}

// ---------------- session ----------------

#[derive(Default)]
struct Stat {
    sum: f64,
    n: u64,
    min: f64,
    max: f64,
}
impl Stat {
    fn add(&mut self, v: f64) {
        if self.n == 0 {
            self.min = v;
            self.max = v;
        } else {
            self.min = self.min.min(v);
            self.max = self.max.max(v);
        }
        self.sum += v;
        self.n += 1;
    }
    fn json(&self) -> String {
        if self.n == 0 {
            return "null".into();
        }
        format!("{{\"avg\":{:.1},\"min\":{:.1},\"max\":{:.1}}}", self.sum / self.n as f64, self.min, self.max)
    }
}

struct Session {
    pkg: String,
    base: String,
    csv: fs::File,
    start: Instant,
    start_epoch: u64,
    rows: usize,
    fps: Stat,
    cpu_t: Stat,
    bat_t: Stat,
    gpu_l: Stat,
    cpu_l: Stat,
    bat_start: i64,
    bat_end: i64,
}

impl Session {
    fn csv_path(&self) -> String {
        format!("{}/{}.csv", dir(), self.base)
    }
    fn json_path(&self) -> String {
        format!("{}/{}.json", dir(), self.base)
    }

    fn sample(&mut self, prev: &mut Option<(u64, u64)>) {
        let t = self.start.elapsed().as_secs();
        let fps = read_fps();
        let cpu_t = cpu_temp_c();
        let bat_t = read_i64("/sys/class/power_supply/battery/temp").map(|v| v as f64 / 10.0);
        let gpu_f = gpu_freq_khz();
        let gpu_ld = gpu_load();
        let (fl, fm, fb) = cpu_freqs();
        let now = cpu_stat();
        let cpu_ld = match (*prev, now) {
            (Some((pt, pi)), Some((nt, ni))) if nt > pt => (100 * ((nt - pt) - ni.saturating_sub(pi).min(nt - pt)) / (nt - pt)) as i64,
            _ => -1,
        };
        *prev = now;
        let ma = read_i64("/sys/class/power_supply/battery/current_now").map(|v| v / 1000);
        let mv = read_i64("/sys/class/power_supply/battery/voltage_now").map(|v| v / 1000);
        let lvl = read_i64("/sys/class/power_supply/battery/capacity").unwrap_or(-1);

        if fps >= 0 { self.fps.add(fps as f64); }
        if let Some(v) = cpu_t { self.cpu_t.add(v); }
        if let Some(v) = bat_t { self.bat_t.add(v); }
        if gpu_ld >= 0 { self.gpu_l.add(gpu_ld as f64); }
        if cpu_ld >= 0 { self.cpu_l.add(cpu_ld as f64); }
        if self.rows == 0 { self.bat_start = lvl; }
        self.bat_end = lvl;

        let f1 = |v: Option<f64>| v.map(|x| format!("{x:.1}")).unwrap_or_else(|| "-1".into());
        let row = format!(
            "{t},{fps},-1,-1,-1,{},{},{gpu_f},{gpu_ld},{fl},{fm},{fb},{cpu_ld},{},{},{lvl}\n",
            f1(cpu_t), f1(bat_t), ma.unwrap_or(-1), mv.unwrap_or(-1)
        );
        let _ = self.csv.write_all(row.as_bytes());
        self.rows += 1;
        if self.rows % 5 == 0 {
            let _ = self.csv.flush();
        }
    }

    fn summary(&self, secs: u64, incomplete: bool) -> String {
        format!(
            "{{\"package\":\"{}\",\"start_epoch\":{},\"duration_s\":{},\"samples\":{},\"incomplete\":{},\
\"fps\":{},\"cpu_temp\":{},\"bat_temp\":{},\"gpu_load\":{},\"cpu_load\":{},\
\"battery\":{{\"start\":{},\"end\":{}}},\"low1\":null,\"low01\":null,\"jank\":null,\
\"units\":{{\"fps\":\"frames/s\",\"temp\":\"C\",\"gpu_freq\":\"kHz\",\"cpu_freq\":\"kHz\",\"load\":\"percent\",\"ma\":\"mA\",\"mv\":\"mV\"}}}}\n",
            self.pkg, self.start_epoch, secs, self.rows, incomplete,
            self.fps.json(), self.cpu_t.json(), self.bat_t.json(), self.gpu_l.json(), self.cpu_l.json(),
            self.bat_start, self.bat_end
        )
    }
}

fn set_state(body: &str) {
    write_atomic(&state_path(), &format!("{body}\n"));
}

fn begin() -> Option<Session> {
    let pkg = active_pkg()?;
    let _ = fs::create_dir_all(dir());
    let _ = fs::set_permissions(dir(), fs::Permissions::from_mode(0o755));
    let base = format!("{pkg}_{}", stamp());
    let path = format!("{}/{base}.csv", dir());
    let mut csv = fs::File::create(&path).ok()?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o644));
    let _ = csv.write_all(format!("{CSV_HEADER}\n").as_bytes());
    let _ = csv.flush();
    set_state(&format!("recording {base}.csv"));
    info!(comp::DAEMON, "recorder: started {base}");
    Some(Session {
        pkg, base, csv,
        start: Instant::now(),
        start_epoch: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
        rows: 0,
        fps: Stat::default(), cpu_t: Stat::default(), bat_t: Stat::default(), gpu_l: Stat::default(), cpu_l: Stat::default(),
        bat_start: -1, bat_end: -1,
    })
}

fn finish(mut s: Session, why: &str) {
    let _ = s.csv.flush();
    let secs = s.start.elapsed().as_secs();
    if secs < MIN_SESSION_SECS {
        let _ = fs::remove_file(s.csv_path());
        info!(comp::DAEMON, "recorder: {} too short ({secs}s, {why}), discarded", s.base);
    } else {
        let body = s.summary(secs, false);
        if fs::write(s.json_path(), body).is_ok() {
            let _ = fs::set_permissions(s.json_path(), fs::Permissions::from_mode(0o644));
        }
        info!(comp::DAEMON, "recorder: {} saved ({secs}s, {why})", s.base);
    }
    set_state("idle");
    prune();
}

fn prune() {
    let mut files: Vec<(SystemTime, String)> = fs::read_dir(dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().map(|x| x == "csv").unwrap_or(false))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path().to_string_lossy().into_owned())))
        .collect();
    if files.len() <= KEEP_SESSIONS {
        return;
    }
    files.sort_by_key(|(t, _)| *t);
    for (_, p) in files.iter().take(files.len() - KEEP_SESSIONS) {
        let _ = fs::remove_file(p);
        let _ = fs::remove_file(Path::new(p).with_extension("json"));
    }
}

/// A session CSV with no JSON means the daemon died mid-recording: mark it so
/// the app reports "incomplete" instead of "saved".
fn recover_leftovers() {
    for e in fs::read_dir(dir()).into_iter().flatten().flatten() {
        let p = e.path();
        if p.extension().map(|x| x == "csv").unwrap_or(false) && !p.with_extension("json").exists() {
            let body = "{\"incomplete\":true}\n";
            let _ = fs::write(p.with_extension("json"), body);
            let _ = fs::set_permissions(p.with_extension("json"), fs::Permissions::from_mode(0o644));
            warn!(comp::DAEMON, "recorder: recovered leftover session {}", p.display());
        }
    }
}

pub(crate) fn start() {
    thread::spawn(run);
}

fn run() {
    let _ = fs::create_dir_all(dir());
    recover_leftovers();
    // the overlay (system uid) must be able to write the command file
    if !Path::new(&cmd_path()).exists() {
        let _ = fs::write(cmd_path(), "");
    }
    let _ = fs::set_permissions(cmd_path(), fs::Permissions::from_mode(0o666));
    set_state("idle");

    let mut sess: Option<Session> = None;
    let mut prev_cpu = cpu_stat();
    let mut last_cmd = Instant::now() - DEBOUNCE;
    let mut tick: u64 = 0;

    loop {
        thread::sleep(Duration::from_millis(500));
        tick += 1;

        if let Some(text) = read_trim(&cmd_path()) {
            let _ = fs::write(cmd_path(), "");
            if last_cmd.elapsed() >= DEBOUNCE {
                last_cmd = Instant::now();
                match (text.as_str(), sess.is_some()) {
                    ("start", false) => sess = begin(),
                    ("stop", true) => {
                        if let Some(s) = sess.take() {
                            finish(s, "manual");
                        }
                    }
                    _ => {}
                }
            }
        }

        if sess.is_some() && active_pkg().is_none() {
            if let Some(s) = sess.take() {
                finish(s, "game closed");
            }
        }

        if tick % 2 == 0 {
            if let Some(s) = sess.as_mut() {
                s.sample(&mut prev_cpu);
            }
        }
    }
}
