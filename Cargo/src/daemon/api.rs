// ---------------- hosted service: vendor.azenith.IAZenithService ----------------
// rsbinder talks to the kernel binder driver directly, so this needs no
// libbinder_ndk and no hand-rolled parcel code. Transaction codes come from the
// generated bindings; the .aidl under Cargo/aidl is the single source of truth
// and must be appended to in order (declaration order == transaction order).

use rsbinder::{hub, BinderResult, Interface};

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

use vendor::azenith::IAZenithService::{BnAZenithService, IAZenithService};

use crate::common::log::{comp, error, info};
use crate::common::paths::STATE_FILE;
use crate::daemon::applist;

pub(crate) const IFACE_VERSION: i32 = 4;

const SERVICE_NAME: &str = "vendor.azenith.IAZenithService/default";

struct AZenithService;

impl Interface for AZenithService {}

fn is_valid_config_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl IAZenithService for AZenithService {
    fn getApiVersion(&self) -> BinderResult<i32> {
        Ok(IFACE_VERSION)
    }

    fn setDisabled(&self, disabled: bool) -> BinderResult<bool> {
        let v = if disabled { "1" } else { "0" };
        crate::common::prop::setprop("persist.sys.azenithIntg.disabletweak", v);
        crate::daemon::profiler::refresh_now("disabletweak changed");
        Ok(true)
    }

    fn isDisabled(&self) -> BinderResult<bool> {
        Ok(crate::common::prop::getprop("persist.sys.azenithIntg.disabletweak") == "1")
    }

    fn setConfig(&self, key: &str, value: &str) -> BinderResult<bool> {
        if !is_valid_config_key(key) {
            error!(comp::DAEMON, "aidl: rejected config key '{key}'");
            return Ok(false);
        }
        crate::common::prop::setprop(&format!("persist.sys.azenithConfIntg.{key}"), value);
        if key == "manual_profile" || key == "litemode" {
            crate::daemon::profiler::refresh_now("config updated");
        }
        Ok(true)
    }

    fn addGame(&self, pkg: &str) -> BinderResult<bool> {
        Ok(!pkg.is_empty() && applist::set_game(pkg, true))
    }

    fn removeGame(&self, pkg: &str) -> BinderResult<bool> {
        Ok(!pkg.is_empty() && applist::set_game(pkg, false))
    }

    fn getGames(&self) -> BinderResult<String> {
        Ok(applist::games().join("\n"))
    }

    fn getState(&self) -> BinderResult<String> {
        Ok(std::fs::read_to_string(STATE_FILE).unwrap_or_default())
    }

    fn getConfig(&self, key: &str) -> BinderResult<String> {
        if !is_valid_config_key(key) {
            error!(comp::DAEMON, "aidl: rejected getConfig key '{key}'");
            return Ok(String::new());
        }
        Ok(crate::common::prop::getprop(&format!("persist.sys.azenithConfIntg.{key}")))
    }

    fn getGameConfigs(&self) -> BinderResult<String> {
        Ok(applist::game_configs_json())
    }

    fn setGameConfig(&self, pkg: &str, config_json: &str) -> BinderResult<bool> {
        Ok(!pkg.is_empty() && applist::set_game_config(pkg, config_json))
    }

    fn removeGameConfig(&self, pkg: &str) -> BinderResult<bool> {
        Ok(!pkg.is_empty() && applist::set_game(pkg, false))
    }

    fn rescanGames(&self) -> BinderResult<bool> {
        Ok(applist::rescan_games())
    }
}

pub(crate) fn start_binder_server() {
    rsbinder::ProcessState::init_default()
        .expect("Failed to initialize binder ProcessState");

    let binder = BnAZenithService::new_binder(AZenithService);
    match hub::add_service(SERVICE_NAME, binder.as_binder()) {
        Ok(()) => info!(comp::DAEMON, "aidl: service registered as {SERVICE_NAME}"),
        Err(e) => error!(comp::DAEMON, "aidl: add_service failed: {e}"),
    }
    rsbinder::ProcessState::start_thread_pool();
}
