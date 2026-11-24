//! Bypass charging for the integrated daemon.
//!
//! Ported (reduced) from `archdaemon/src/bypass.rs` of AZenith main so the
//! GameSpace app can drive the same flows without root:
//!
//! | CLI flag of main | Task here | Output the app parses |
//! |---|---|---|
//! | `-bpl` | `bypass_list`    | rows `NAME | [FOUND] | path` |
//! | `-cbc` | `bypass_check`   | `Found working node: NAME` |
//! | `-tb`  | `bypass_test`    | `Node is compatible: NAME` |
//! | `-rp`  | `bypass_restore` | `Restored a charge node ...` |
//!
//! Runtime bypass (`reevaluate`) is event driven: it is called when the
//! profile or the battery state changes, never from a timer.
//!
//! Differences from main (kept on purpose, the daemon is built without a
//! device to test on): the node test is a simpler "battery current must stay
//! low for the whole window and recover after the restore" check, and the
//! write-ahead record is a single file in `API_DIR`.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use crate::common::android_ffi::prop_get;
use crate::common::log::{comp, info, warn};
use crate::common::paths::API_DIR;

const PROP_ENABLE: &str = "persist.sys.azenithConfIntg.bypasschg";
const PROP_PATH: &str = "persist.sys.azenithConfIntg.bypasspath";
const PROP_THRESHOLD: &str = "persist.sys.azenithConfIntg.bypasschgthreshold";

pub(crate) struct Node {
    pub(crate) name: &'static str,
    pub(crate) path: &'static str,
    pub(crate) on: &'static str,
    pub(crate) off: &'static str,
}

/// Same table as `BYPASS_LIST` in AZenith main (generated from it).
pub(crate) const LIST: &[Node] = &[
    Node { name: "COMMON_INPUT_SUSPEND", path: "/sys/class/power_supply/battery/input_suspend", on: "1", off: "0" },
    Node { name: "COMMON_BATT_INPUT_SUSPEND", path: "/sys/class/power_supply/battery/battery_input_suspend", on: "1", off: "0" },
    Node { name: "COMMON_CHG_CONTROL", path: "/sys/class/power_supply/battery/charger_control", on: "0", off: "1" },
    Node { name: "COMMON_CHG_DISABLE", path: "/sys/class/power_supply/battery/charge_disable", on: "1", off: "0" },
    Node { name: "COMMON_CHG_ENABLED_V1", path: "/sys/class/power_supply/battery/charging_enabled", on: "0", off: "1" },
    Node { name: "COMMON_CHG_ENABLED_V2", path: "/sys/class/power_supply/battery/charge_enabled", on: "0", off: "1" },
    Node { name: "COMMON_BATT_CHG_ENABLED", path: "/sys/class/power_supply/battery/battery_charging_enabled", on: "0", off: "1" },
    Node { name: "COMMON_DEVICE_CHG_EN", path: "/sys/class/power_supply/battery/device/Charging_Enable", on: "0", off: "1" },
    Node { name: "MTK_BYPASS_CHG", path: "/sys/devices/platform/charger/bypass_charger", on: "1", off: "0" },
    Node { name: "MTK_CURRENT_CMD", path: "/proc/mtk_battery_cmd/current_cmd", on: "0 1", off: "0 0" },
    Node { name: "TRAN_AICHG_DISABLE", path: "/sys/devices/platform/charger/tran_aichg_disable_charger", on: "1", off: "0" },
    Node { name: "MTK_DISABLE_BATTERY_CHG", path: "/sys/devices/platform/mt-battery/disable_charger", on: "1", off: "0" },
    Node { name: "MTK_ADV_PATH", path: "/proc/mtk_battery_cmd/en_power_path", on: "0", off: "1" },
    Node { name: "OPLUS_MMI_1", path: "/sys/class/oplus_chg/battery/mmi_charging_enable", on: "0", off: "1" },
    Node { name: "OPLUS_MMI_2", path: "/sys/class/power_supply/battery/mmi_charging_enable", on: "0", off: "1" },
    Node { name: "OPLUS_MMI_3", path: "/sys/devices/virtual/oplus_chg/battery/mmi_charging_enable", on: "0", off: "1" },
    Node { name: "OPLUS_MMI_SOC", path: "/sys/devices/platform/soc/soc:oplus,chg_intf/oplus_chg/battery/mmi_charging_enable", on: "0", off: "1" },
    Node { name: "OPLUS_EXP_CHG_ENABLE", path: "/sys/devices/platform/soc/soc:oplus,chg_intf/oplus_chg/battery/chg_enable", on: "0", off: "1" },
    Node { name: "OPLUS_COOLDOWN_STATE", path: "/sys/devices/platform/soc/soc:oplus,chg_intf/oplus_chg/battery/cool_down", on: "1", off: "0" },
    Node { name: "AC_CHG_ENABLED", path: "/sys/class/power_supply/ac/charging_enabled", on: "0", off: "1" },
    Node { name: "CHG_DATA_ENABLE", path: "/sys/class/power_supply/charge_data/enable_charger", on: "0", off: "1" },
    Node { name: "DC_CHG_ENABLED", path: "/sys/class/power_supply/dc/charging_enabled", on: "0", off: "1" },
    Node { name: "OP_DISABLE_CHG", path: "/sys/class/power_supply/battery/op_disable_charge", on: "1", off: "0" },
    Node { name: "CHGALG_DISABLE_CHG", path: "/sys/class/power_supply/chargalg/disable_charging", on: "1", off: "0" },
    Node { name: "BATT_CONNECT_DISABLE", path: "/sys/class/power_supply/battery/connect_disable", on: "1", off: "0" },
    Node { name: "I2C_CHG_ENABLE", path: "/sys/devices/platform/omap/omap_i2c.3/i2c-3/3-005f/charge_enable", on: "0", off: "1" },
    Node { name: "QPNP_SMB_BATT_EN", path: "/sys/devices/soc/qpnp-smbcharger-18/power_supply/battery/battery_charging_enabled", on: "0", off: "1" },
    Node { name: "QCOM_SUSPEND", path: "/sys/class/qcom-battery/input_suspend", on: "1", off: "0" },
    Node { name: "QCOM_EN_CHG", path: "/sys/class/qcom-battery/charging_enabled", on: "0", off: "1" },
    Node { name: "QCOM_COOL_MODE", path: "/sys/class/qcom-battery/cool_mode", on: "1", off: "0" },
    Node { name: "QCOM_PROTECT_EN", path: "/sys/class/qcom-battery/batt_protect_en", on: "1", off: "0" },
    Node { name: "QCOM_BATT_PROTECTED", path: "/sys/class/qcom-battery/battery_protected", on: "1", off: "0" },
    Node { name: "QCOM_PMIC_GLINK_SUSPEND", path: "/sys/devices/platform/soc/soc:qcom,pmic_glink/soc:qcom,pmic_glink:qcom,battery_charger/force_charger_suspend", on: "1", off: "0" },
    Node { name: "PM8058_DISABLE", path: "/sys/module/pmic8058_charger/parameters/disabled", on: "1", off: "0" },
    Node { name: "PM8921_DISABLE", path: "/sys/module/pm8921_charger/parameters/disabled", on: "1", off: "0" },
    Node { name: "SMB137B_DISABLE", path: "/sys/module/smb137b/parameters/disabled", on: "1", off: "0" },
    Node { name: "SMB1357_DISABLE_PROC", path: "/proc/smb1357_disable_chrg", on: "1", off: "0" },
    Node { name: "BQ2589X_EN_CHG", path: "/sys/class/power_supply/bq2589x_charger/enable_charging", on: "0", off: "1" },
    Node { name: "PIXEL_CHG_DISABLE", path: "/sys/devices/platform/soc/soc:google,charger/charge_disable", on: "1", off: "0" },
    Node { name: "PIXEL_DEBUG_SUSPEND", path: "/sys/kernel/debug/google_charger/chg_suspend", on: "1", off: "0" },
    Node { name: "PIXEL_INPUT_SUSPEND", path: "/sys/kernel/debug/google_charger/input_suspend", on: "1", off: "0" },
    Node { name: "PIXEL_STOP_LEVEL", path: "/sys/devices/platform/google,charger/charge_stop_level", on: "5", off: "100" },
    Node { name: "PIXEL_CHG_MODE", path: "/sys/kernel/debug/google_charger/chg_mode", on: "0", off: "1" },
    Node { name: "SAM_STORE_MODE", path: "/sys/class/power_supply/battery/store_mode", on: "1", off: "0" },
    Node { name: "LGE_CHG_ENABLE", path: "/sys/devices/platform/lge-unified-nodes/charging_enable", on: "0", off: "1" },
    Node { name: "LGE_CHG_COMPLETED", path: "/sys/devices/platform/lge-unified-nodes/charging_completed", on: "1", off: "0" },
    Node { name: "LGE_STOP_LEVEL", path: "/sys/module/lge_battery/parameters/charge_stop_level", on: "5", off: "100" },
    Node { name: "ASUS_LIMIT_EN", path: "/sys/class/asuslib/charger_limit_en", on: "1", off: "0" },
    Node { name: "ASUS_SUSPEND_EN", path: "/sys/class/asuslib/charging_suspend_en", on: "1", off: "0" },
    Node { name: "HUAWEI_CHG_EN_1", path: "/sys/devices/platform/huawei_charger/enable_charger", on: "0", off: "1" },
    Node { name: "HUAWEI_CHG_EN_2", path: "/sys/class/hw_power/charger/charge_data/enable_charger", on: "0", off: "1" },
    Node { name: "NUBIA_BYPASS_MODE", path: "/sys/kernel/nubia_charge/charger_bypass", on: "on", off: "off" },
    Node { name: "MANTA_CHG_EN", path: "/sys/devices/virtual/power_supply/manta-battery/charge_enabled", on: "0", off: "1" },
    Node { name: "CAT_CHG_SWITCH", path: "/sys/devices/platform/battery/CCIChargerSwitch", on: "0", off: "1" },
    Node { name: "SPREADTRUM_STOP_CHG", path: "/sys/class/power_supply/battery/stop_charge", on: "1", off: "0" },
    Node { name: "TEGRA_I2C_STATE", path: "/sys/devices/platform/tegra12-i2c.0/i2c-0/0-006b/charging_state", on: "disabled", off: "enabled" },
    Node { name: "SIOP_LEVEL_CTRL", path: "/sys/class/power_supply/battery/siop_level", on: "0", off: "100" },
    Node { name: "SMART_INTERRUPT_CHG", path: "/sys/class/power_supply/battery_ext/smart_charging_interruption", on: "1", off: "0" },
    Node { name: "CHG_LIMIT_ENABLE", path: "/proc/driver/charger_limit_enable", on: "1", off: "0" },
    Node { name: "CHG_LIMIT_VAL", path: "/proc/driver/charger_limit", on: "5", off: "100" },
    Node { name: "QPNP_ADAPTIVE_BLOCK", path: "/sys/module/qpnp_adaptive_charge/parameters/blocking", on: "1", off: "0" },
    Node { name: "BATT_TEST_MODE", path: "/sys/class/power_supply/battery/test_mode", on: "1", off: "2" },
    Node { name: "BATT_SLATE_MODE", path: "/sys/class/power_supply/battery/batt_slate_mode", on: "1", off: "0" },
    Node { name: "BATT_DEFENDER_CNT", path: "/sys/class/power_supply/battery/bd_trickle_cnt", on: "1", off: "0" },
    Node { name: "IDT_PIN_EN", path: "/sys/class/power_supply/idt/pin_enabled", on: "1", off: "0" },
    Node { name: "CHG_STATE_CTRL", path: "/sys/class/power_supply/battery/charge_charger_state", on: "1", off: "0" },
    Node { name: "ADAPTER_CC_MODE", path: "/sys/class/power_supply/main/adapter_cc_mode", on: "1", off: "0" },
    Node { name: "HMT_TA_CHG", path: "/sys/class/power_supply/battery/hmt_ta_charge", on: "0", off: "1" },
    Node { name: "MAXFG_OFF_CHG", path: "/sys/class/power_supply/maxfg/offmode_charger", on: "1", off: "0" },
    Node { name: "COOL_MODE_MAIN", path: "/sys/class/power_supply/main/cool_mode", on: "1", off: "0" },
    Node { name: "RESTRICTED_CHG_BATT", path: "/sys/class/power_supply/battery/restricted_charging", on: "1", off: "0" },
    Node { name: "RESTRICTED_CHG_WIRELESS", path: "/sys/class/power_supply/wireless/restricted_charging", on: "1", off: "0" },
    Node { name: "XIAOMI_MCA_INPUT_SUSPEND_V1", path: "/sys/devices/platform/soc/soc:mca_charge_interface/input_suspend", on: "1", off: "0" },
    Node { name: "XIAOMI_MCA_CHARGE_ENABLE_V1", path: "/sys/devices/platform/soc/soc:mca_charge_interface/charge_enable", on: "0", off: "1" },
    Node { name: "XIAOMI_MCA_STOP_HANDLE_V1", path: "/sys/devices/platform/soc/soc:mca_business_charger/stop_handle_charge", on: "1", off: "0" },
    Node { name: "XIAOMI_XM_INPUT_SUSPEND", path: "/sys/class/xm_power/charger/charge_interface/input_suspend", on: "1", off: "0" },
    Node { name: "XIAOMI_XM_STOP_HANDLE", path: "/sys/class/xm_power/charger/charger_common/stop_handle_charge", on: "1", off: "0" },
    Node { name: "XIAOMI_XM_CHARGE_ENABLE", path: "/sys/class/xm_power/charger/charge_interface/charge_enable", on: "0", off: "1" },
];

macro_rules! say {
    ($o:expr, $($a:tt)*) => {{
        let _ = writeln!($o, $($a)*);
        let _ = $o.flush();
    }};
}

fn prop(key: &str) -> String {
    prop_get(key).unwrap_or_default()
}

fn exists(path: &str) -> bool {
    Path::new(path).exists()
}

fn pending_file() -> String {
    format!("{}/bypass_pending", API_DIR)
}

// ---------------------------------------------------------------------------
// sysfs helpers
// ---------------------------------------------------------------------------

/// Write the way `echo` does (with the trailing newline): a few vendor drivers
/// only accept the terminated form.
fn write_node(path: &str, value: &str) -> std::io::Result<()> {
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o644));
    fs::write(path, format!("{value}\n"))
}

fn read_first_line(path: &str) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
        .filter(|s| !s.is_empty())
}

const CHARGER_ONLINE: [&str; 5] = [
    "/sys/class/power_supply/charger/online",
    "/sys/class/power_supply/usb/online",
    "/sys/class/power_supply/usb_switch/online",
    "/sys/class/power_supply/ac/online",
    "/sys/class/power_supply/dc/online",
];
const BATTERY_CURRENT: [&str; 3] = [
    "/sys/class/power_supply/battery/current_now",
    "/sys/class/power_supply/battery/current_avg",
    "/sys/class/power_supply/battery/BatteryAverageCurrent",
];
const INPUT_CURRENT: [&str; 3] = [
    "/sys/class/power_supply/usb/current_now",
    "/sys/class/power_supply/ac/current_now",
    "/sys/class/power_supply/dc/current_now",
];

/// Unit the driver reports current in, learned from the first unambiguous
/// reading (10 A never flows through a phone battery, 10 mA in uA does).
static MICRO: AtomicBool = AtomicBool::new(false);

fn read_ma(path: &str) -> Option<i32> {
    let raw = read_first_line(path)?;
    let v: i64 = raw.parse().ok()?;
    let abs = v.abs();
    if abs >= 10_000 {
        MICRO.store(true, Ordering::Relaxed);
    }
    Some(if MICRO.load(Ordering::Relaxed) { (abs / 1000) as i32 } else { abs as i32 })
}

fn battery_ma() -> Option<i32> {
    BATTERY_CURRENT.iter().find_map(|p| read_ma(p))
}

fn input_ma() -> Option<i32> {
    INPUT_CURRENT.iter().find_map(|p| read_ma(p))
}

fn battery_status() -> Option<String> {
    read_first_line("/sys/class/power_supply/battery/status")
}

/// A charger is attached. Not the same as "charging": a phone in bypass
/// reports "Not charging" (sometimes "Discharging") while still plugged in.
fn plugged() -> bool {
    if CHARGER_ONLINE.iter().any(|p| read_first_line(p).as_deref() == Some("1")) {
        return true;
    }
    match battery_status() {
        Some(s) => !s.eq_ignore_ascii_case("Discharging"),
        None => false,
    }
}

fn median(v: &mut [i32]) -> i32 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

fn sleep_ms(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}

// ---------------------------------------------------------------------------
// write-ahead restore record
// ---------------------------------------------------------------------------

struct Pending {
    name: String,
    path: String,
    off: String,
    orig: Option<String>,
    owner: String, // "test" | "runtime"
}

impl Pending {
    fn restore_value(&self) -> &str {
        self.orig.as_deref().unwrap_or(&self.off)
    }
}

fn write_pending(node: &Node, orig: &Option<String>, owner: &str) -> bool {
    let body = format!(
        "name={}\npath={}\noff={}\norig={}\nowner={}\n",
        node.name,
        node.path,
        node.off,
        orig.as_deref().unwrap_or(""),
        owner
    );
    fs::write(pending_file(), body).is_ok()
}

fn read_pending() -> Option<Pending> {
    let text = fs::read_to_string(pending_file()).ok()?;
    let (mut name, mut path, mut off, mut orig, mut owner) = (None, None, None, None, String::from("test"));
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim().to_string();
        match k.trim() {
            "name" => name = Some(v),
            "path" => path = Some(v),
            "off" => off = Some(v),
            "orig" => orig = Some(v).filter(|s| !s.is_empty()),
            "owner" => owner = v,
            _ => {}
        }
    }
    Some(Pending { name: name?, path: path?, off: off?, orig, owner })
}

fn clear_pending() {
    let _ = fs::remove_file(pending_file());
}

/// `-rp`: undo a node left ON by an interrupted test or a daemon crash.
/// A node the running daemon applied itself (runtime bypass) is left alone,
/// otherwise opening the diagnosis screen would cut bypass mid-game.
pub(crate) fn restore_pending(o: &mut dyn Write) -> i32 {
    let Some(p) = read_pending() else { return 0 };
    if p.owner == "runtime" && APPLIED.load(Ordering::Relaxed) {
        return 0;
    }
    match write_node(&p.path, p.restore_value()) {
        Ok(()) => {
            clear_pending();
            say!(o, "[INFO] Restored a charge node left ON by an interrupted test: {}", p.name);
            info!(comp::BYPASS, "restored pending node {}", p.name);
            0
        }
        Err(e) => {
            say!(o, "[WARN] Could not restore {} ({e}); reboot if charging does not resume", p.name);
            warn!(comp::BYPASS, "restore of {} failed: {e}", p.name);
            1
        }
    }
}

// ---------------------------------------------------------------------------
// node test
// ---------------------------------------------------------------------------

const SETTLE_MS: u64 = 2000;
const SAMPLE_MS: u64 = 500;
const SAMPLES: usize = 16;
const LOW_MA: i32 = 50;
const LOW_RUN: usize = 3;
const MIN_TESTABLE_MA: i32 = 100;
const RECOVERY_MAX_MS: u64 = 5000;
const RECOVERY_STEP_MS: u64 = 250;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    WriteFailed,
    UnreadableCurrent,
    NoBaseline(i32),
    CurrentNotDropped(i32),
    NodeReverted(i32),
    InputLost(i32, i32),
    RestoreFailed(i32),
    NoRecovery(i32),
}

impl Verdict {
    fn describe(self) -> String {
        match self {
            Verdict::Pass => "bypass verified".into(),
            Verdict::WriteFailed => "could not write to node".into(),
            Verdict::UnreadableCurrent => "battery current unreadable".into(),
            Verdict::NoBaseline(ma) => format!("battery was already at {ma} mA before the test: nothing to measure, node left untouched"),
            Verdict::CurrentNotDropped(ma) => format!("current did not drop ({ma} mA)"),
            Verdict::NodeReverted(ma) => format!("node was reset to its OFF value while testing ({ma} mA): the charger HAL or kernel overrides it"),
            Verdict::InputLost(inp, bat) => format!("charger input collapsed too ({inp} mA in, {bat} mA to battery): charging was killed, not bypassed"),
            Verdict::RestoreFailed(ma) => format!("restore write failed while current was {ma} mA: node may still be latched, reboot if charging does not resume"),
            Verdict::NoRecovery(ma) => format!("current did not recover after restore ({ma} mA): node may still be latched"),
        }
    }
}

struct Outcome {
    verdict: Verdict,
    baseline: i32,
    held: i32,
    steady: i32,
    restored: i32,
}

impl Outcome {
    fn fail(verdict: Verdict, baseline: i32) -> Self {
        Self { verdict, baseline, held: 0, steady: 0, restored: 0 }
    }
}

fn longest_low_run(samples: &[i32]) -> usize {
    let (mut best, mut cur) = (0, 0);
    for &s in samples {
        if s < LOW_MA {
            cur += 1;
            best = best.max(cur);
        } else {
            cur = 0;
        }
    }
    best
}

fn baseline_ma() -> Option<i32> {
    let mut v = Vec::new();
    for _ in 0..3 {
        v.push(battery_ma()?);
        sleep_ms(300);
    }
    Some(median(&mut v))
}

fn test_node(o: &mut dyn Write, node: &Node, verbose: bool) -> Outcome {
    let Some(baseline) = baseline_ma() else {
        return Outcome::fail(Verdict::UnreadableCurrent, 0);
    };
    if baseline < MIN_TESTABLE_MA {
        return Outcome::fail(Verdict::NoBaseline(baseline), baseline);
    }
    let input_before = input_ma();
    let orig = read_first_line(node.path).filter(|v| v != node.on);
    let restore_val = orig.clone().unwrap_or_else(|| node.off.to_string());

    // The record is written before the node is touched: a SIGKILL or reboot
    // mid-test would otherwise leave charging latched off.
    if !write_pending(node, &orig, "test") {
        return Outcome::fail(Verdict::WriteFailed, baseline);
    }
    if write_node(node.path, node.on).is_err() {
        let _ = write_node(node.path, &restore_val);
        clear_pending();
        return Outcome::fail(Verdict::WriteFailed, baseline);
    }

    sleep_ms(SETTLE_MS);
    let mut samples = Vec::with_capacity(SAMPLES);
    let mut inputs = Vec::new();
    for i in 0..SAMPLES {
        sleep_ms(SAMPLE_MS);
        let cur = battery_ma().unwrap_or(9999);
        samples.push(cur);
        if let Some(inp) = input_ma() {
            inputs.push(inp);
        }
        if verbose {
            say!(o, "    [{:>2}/{}] battery {} mA", i + 1, SAMPLES, cur);
        }
    }
    let readback = read_first_line(node.path);

    let restore_ok = write_node(node.path, &restore_val).is_ok();
    if restore_ok {
        clear_pending();
    }

    // recovery: the charge rate must come back after the restore
    let target = (baseline / 4).max(MIN_TESTABLE_MA);
    let mut recovered = 0;
    let mut waited = 0;
    while waited <= RECOVERY_MAX_MS {
        recovered = battery_ma().unwrap_or(0);
        if recovered >= target {
            break;
        }
        sleep_ms(RECOVERY_STEP_MS);
        waited += RECOVERY_STEP_MS;
    }

    let low = samples.iter().filter(|&&s| s < LOW_MA).count();
    let steady = (low * 100 / samples.len().max(1)) as i32;
    let mut sorted = samples.clone();
    let held = median(&mut sorted);
    let restored_pct = ((recovered as i64 * 100) / baseline.max(1) as i64).clamp(0, 100) as i32;

    let verdict = if !restore_ok {
        Verdict::RestoreFailed(held)
    } else if longest_low_run(&samples) < LOW_RUN || steady < 75 {
        if readback.as_deref() == Some(node.off) {
            Verdict::NodeReverted(held)
        } else {
            Verdict::CurrentNotDropped(held)
        }
    } else if let (Some(before), false) = (input_before, inputs.is_empty()) {
        let mut inp = inputs.clone();
        let m = median(&mut inp);
        if before >= MIN_TESTABLE_MA && m < LOW_MA {
            Verdict::InputLost(m, held)
        } else if recovered < target {
            Verdict::NoRecovery(recovered)
        } else {
            Verdict::Pass
        }
    } else if recovered < target {
        Verdict::NoRecovery(recovered)
    } else {
        Verdict::Pass
    };

    Outcome { verdict, baseline, held, steady, restored: restored_pct }
}

/// Same gate as main: a charger must be attached or there is nothing to test.
fn preflight(o: &mut dyn Write) -> bool {
    if !plugged() {
        say!(o, "[!] WARNING: charger is not connected. Plug in a charger and try again.");
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

/// `-bpl`
pub(crate) fn list(o: &mut dyn Write) -> i32 {
    say!(o, "");
    say!(o, "[AZenith Available Bypass Path List]");
    say!(o, "---------------------------------------------------------------------------------------");
    say!(o, " {:<30} | {:<10} | {}", "NODE NAME", "STATUS", "SYSFS/PROC PATH");
    say!(o, "---------------------------------------------------------------------------------------");
    let mut found = 0;
    for n in LIST {
        if exists(n.path) {
            say!(o, " {:<30} | [FOUND]    | {}", n.name, n.path);
            found += 1;
        } else {
            say!(o, " {:<30} | [NOT FOUND] | {}", n.name, n.path);
        }
    }
    say!(o, "---------------------------------------------------------------------------------------");
    say!(o, "[SUMMARY] Total Nodes: {} | Available on this device: {}", LIST.len(), found);
    say!(o, "");
    0
}

/// `-tb <NODE>`
pub(crate) fn test_by_name(o: &mut dyn Write, name: &str) -> i32 {
    let name = name.trim().trim_matches('"').trim_start_matches("--path=");
    if name.is_empty() {
        say!(o, "[ERROR] No bypass node name provided.");
        return 1;
    }
    let Some(node) = LIST.iter().find(|n| n.name == name) else {
        say!(o, "[ERROR] Bypass node not found: {name}");
        say!(o, "[INFO] Use the node list to see available nodes.");
        return 2;
    };
    restore_pending(o);
    say!(o, "");
    say!(o, "[Bypass Charge Test - Single Node] Initializing...");
    if !preflight(o) {
        return -1;
    }
    if !exists(node.path) {
        say!(o, "[ERROR] Node path not found: {}", node.path);
        return 2;
    }
    say!(o, "");
    say!(o, "[+] Testing Node: {}", node.name);
    say!(o, "    Path: {}", node.path);
    let out = test_node(o, node, true);
    if out.verdict == Verdict::Pass {
        say!(
            o,
            "\n[SUCCESS] Node is compatible: {} ({} mA, baseline {} mA, steady {}%, restore {}%)",
            node.name, out.held, out.baseline, out.steady, out.restored
        );
        info!(comp::BYPASS, "single node test success name={} baseline={} held={}", node.name, out.baseline, out.held);
        0
    } else {
        say!(o, "\n[FAILED] {}: {}", node.name, out.verdict.describe());
        warn!(comp::BYPASS, "single node test failed name={}: {}", node.name, out.verdict.describe());
        1
    }
}

/// `-cbc`: test every node that exists on this device and pick the best one.
pub(crate) fn check(o: &mut dyn Write) -> i32 {
    say!(o, "");
    say!(o, "[Bypass Charge Compatibility Check] Initializing...");
    restore_pending(o);
    if !preflight(o) {
        return -1;
    }
    let found: Vec<&Node> = LIST.iter().filter(|n| exists(n.path)).collect();
    say!(o, "[i] {} candidate node(s) exist on this device", found.len());
    if found.is_empty() {
        say!(o, "[!] No bypass node found on this device");
        return 1;
    }

    let mut best: Option<(&Node, (i32, i32))> = None;
    for (i, node) in found.iter().enumerate() {
        say!(o, "");
        say!(o, "[{}/{}] Testing {}", i + 1, found.len(), node.name);
        let out = test_node(o, node, false);
        if out.verdict == Verdict::Pass {
            say!(o, "    -> PASS ({} mA, steady {}%, restore {}%)", out.held, out.steady, out.restored);
            let q = (out.steady, out.restored);
            if best.map(|(_, bq)| q > bq).unwrap_or(true) {
                best = Some((node, q));
            }
        } else {
            say!(o, "    -> FAIL: {}", out.verdict.describe());
        }
        if !plugged() {
            say!(o, "[!] Charger was disconnected, aborting the scan");
            break;
        }
    }

    say!(o, "");
    match best {
        Some((n, _)) => {
            say!(o, "[SUCCESS] Found working node: {}", n.name);
            0
        }
        None => {
            say!(o, "[!] No working bypass node found on this device");
            1
        }
    }
}

// ---------------------------------------------------------------------------
// runtime bypass (driven by profile / battery events)
// ---------------------------------------------------------------------------

static APPLIED: AtomicBool = AtomicBool::new(false);
static RT_LOCK: Mutex<Option<(usize, Option<String>)>> = Mutex::new(None);
/// Serializes `reevaluate`: two overlapping runs could both pass the
/// "not applied yet" check and the second would record the already-ON value as
/// the original, leaving charging latched off.
static EVAL: Mutex<()> = Mutex::new(());

fn enable(idx: usize) -> bool {
    let node = &LIST[idx];
    let orig = read_first_line(node.path).filter(|v| v != node.on);
    if !write_pending(node, &orig, "runtime") {
        return false;
    }
    if write_node(node.path, node.on).is_err() {
        clear_pending();
        return false;
    }
    *RT_LOCK.lock().unwrap_or_else(|e| e.into_inner()) = Some((idx, orig));
    APPLIED.store(true, Ordering::Relaxed);
    true
}

fn disable(slot: &mut Option<(usize, Option<String>)>) {
    if let Some((idx, orig)) = slot.take() {
        let node = &LIST[idx];
        let val = orig.unwrap_or_else(|| node.off.to_string());
        if write_node(node.path, &val).is_err() {
            warn!(comp::BYPASS, "runtime restore of {} failed", node.name);
        }
    }
    clear_pending();
    APPLIED.store(false, Ordering::Relaxed);
}

/// Re-decide whether bypass should be on. Cheap enough to call on every
/// profile / battery event; props are read with bionic, no fork.
pub(crate) fn reevaluate() {
    let _eval = EVAL.lock().unwrap_or_else(|e| e.into_inner());
    let name = prop(PROP_PATH);
    let enabled = prop(PROP_ENABLE) == "1" && !name.is_empty() && name != "UNSUPPORTED";
    let idx = LIST.iter().position(|n| n.name == name);
    let threshold = prop(PROP_THRESHOLD).parse::<i32>().unwrap_or(20);
    let (level, _) = crate::daemon::state::battery();
    let perf = crate::daemon::profiler::cur() == crate::daemon::profiler::MODE_PERF;
    let tweaks_off = prop("persist.sys.azenithIntg.disabletweak") == "1";

    let want = enabled && idx.is_some() && perf && !tweaks_off && level >= threshold && level >= 0 && plugged();

    let mut slot = RT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let applied = APPLIED.load(Ordering::Relaxed);
    if want && !applied {
        // only start when the battery is really charging: nothing to bypass otherwise
        if battery_ma().unwrap_or(0) > LOW_MA {
            drop(slot);
            if let Some(i) = idx {
                if enable(i) {
                    info!(comp::BYPASS, "bypass enabled node={} battery={} threshold={}", LIST[i].name, level, threshold);
                } else {
                    warn!(comp::BYPASS, "bypass enable failed node={}", LIST[i].name);
                }
            }
        }
    } else if !want && applied {
        disable(&mut slot);
        info!(comp::BYPASS, "bypass disabled battery={} threshold={}", level, threshold);
    } else if want && applied {
        // user picked another node while bypass is on: move over
        let cur_idx = slot.as_ref().map(|(c, _)| *c);
        if let (Some(i), Some(c)) = (idx, cur_idx) {
            if i != c {
                disable(&mut slot);
                drop(slot);
                let _ = enable(i);
            }
        }
    }
}

pub(crate) fn reevaluate_async() {
    thread::spawn(reevaluate);
}

/// Called once at daemon start: undo whatever a crashed run left behind.
pub(crate) fn startup_restore() {
    let mut sink = std::io::sink();
    restore_pending(&mut sink);
}

// ---------------------------------------------------------------------------
// entry points
// ---------------------------------------------------------------------------

/// Used by the daemon's task runner (output goes to a file the app tails).
pub(crate) fn run_task(task: &str, arg: &str, o: &mut dyn Write) -> i32 {
    match task {
        "bypass_list" => list(o),
        "bypass_check" => check(o),
        "bypass_test" => test_by_name(o, arg),
        "bypass_restore" => restore_pending(o),
        _ => {
            say!(o, "[ERROR] unknown task: {task}");
            2
        }
    }
}

/// `vendor.azenith-service bypass <list|check|test NODE|restore>` for manual use.
pub(crate) fn run(args: &[String]) {
    let sub = args.first().map(String::as_str).unwrap_or("list");
    let arg = args.get(1).map(String::as_str).unwrap_or("");
    let task = match sub {
        "list" | "-bpl" => "bypass_list",
        "check" | "-cbc" => "bypass_check",
        "test" | "-tb" => "bypass_test",
        "restore" | "-rp" => "bypass_restore",
        other => {
            eprintln!("usage: bypass <list|check|test NODE|restore> (got '{other}')");
            std::process::exit(2);
        }
    };
    let mut out = std::io::stdout();
    let code = run_task(task, arg, &mut out);
    std::process::exit(code.max(0));
}
