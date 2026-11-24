// ---------------- gamelist.txt + game_configs.json (GameRegistry) + inotify ----------------
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::c_void;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::thread;

use crate::common::log::{comp, debug, error, info, warn};
use crate::daemon::state::{adopt, st, write_atomic};

pub(crate) use crate::common::paths::{
    GAME_CONFIGS_FILE as CONFIGS_FILE,
    GAMELIST_DIR as LIST_DIR,
    GAMELIST_FILE as LIST_FILE,
};
const LIST_NAME: &str = "gamelist.txt";

const LOADED_FLAG: &str = "gamelistloaded";
const LOADED_VERSION: &str = "games-v1";

const NEVER_MANAGE: &[&str] = &["zx.azenith"];

const DEFAULT_GAME_CONFIG: &[(&str, &str)] = &[
    ("perf_lite_mode", "default"),
    ("dnd_on_gaming", "default"),
    ("app_priority", "default"),
    ("game_preload", "default"),
    ("refresh_rate", "default"),
    ("renderer", "default"),
    ("resolution_downscale", "default"),
    ("resolution_fps", "60"),
    ("bypass_charging", "default"),
];

type GameConfigMap = BTreeMap<String, BTreeMap<String, String>>;

struct ConfigStore {
    loaded: bool,
    entries: GameConfigMap,
}

static CONFIGS: Mutex<ConfigStore> = Mutex::new(ConfigStore {
    loaded: false,
    entries: BTreeMap::new(),
});

pub(crate) fn is_valid_package(pkg: &str) -> bool {
    let pkg = pkg.trim();
    if pkg.len() < 3 || pkg.len() > 255 || !pkg.contains('.') {
        return false;
    }
    if pkg.starts_with('.') || pkg.ends_with('.') || pkg.contains("..") {
        return false;
    }
    if !pkg.split('.').all(|seg| {
        !seg.is_empty() && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }) {
        return false;
    }
    !NEVER_MANAGE
        .iter()
        .any(|skip| pkg == *skip || pkg.starts_with(&format!("{skip}.")))
}

fn default_entry() -> BTreeMap<String, String> {
    DEFAULT_GAME_CONFIG
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn normalize_entry(raw: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut out = default_entry();
    for (k, v) in raw {
        if let Some(slot) = out.get_mut(k.as_str()) {
            let clean = v.trim();
            if !clean.is_empty()
                && clean.len() <= 64
                && clean.chars().all(|c| !c.is_control())
            {
                *slot = clean.to_string();
            }
        }
    }
    out
}

fn ensure_configs_loaded(store: &mut ConfigStore) {
    if store.loaded {
        return;
    }
    store.loaded = true;
    if let Ok(text) = fs::read_to_string(CONFIGS_FILE) {
        if let Some(parsed) = parse_nested_json(&text) {
            for (pkg, cfg) in parsed {
                if is_valid_package(&pkg) {
                    store.entries.insert(pkg, normalize_entry(&cfg));
                }
            }
        } else {
            warn!(comp::APPLIST, "ignoring malformed {CONFIGS_FILE}");
        }
    }
}

fn persist_configs_locked(entries: &GameConfigMap) -> bool {
    let json = serialize_nested_json(entries);
    write_atomic(CONFIGS_FILE, &json)
}

fn parse_list(text: &str) -> BTreeSet<String> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .map(|l| l.split_whitespace().next().unwrap_or(""))
        .filter(|l| !l.is_empty())
        .map(|l| l.to_string())
        .collect()
}

/// Add or remove one package, persist both gamelist.txt and game_configs.json, and reload.
pub(crate) fn set_game(pkg: &str, enabled: bool) -> bool {
    let pkg = pkg.trim();
    if !is_valid_package(pkg) {
        warn!(comp::APPLIST, "rejected invalid package name '{pkg}'");
        return false;
    }

    let mut list = st().list.clone();
    if enabled {
        list.insert(pkg.to_string());
    } else {
        list.remove(pkg);
    }

    {
        let mut store = CONFIGS.lock().unwrap_or_else(|e| e.into_inner());
        ensure_configs_loaded(&mut store);
        if enabled {
            store.entries.entry(pkg.to_string()).or_insert_with(default_entry);
        } else {
            store.entries.remove(pkg);
        }
        if !persist_configs_locked(&store.entries) {
            return false;
        }
    }

    if !write_list(&list) {
        return false;
    }
    reload_list();
    true
}

/// Update or create a game's per-app configuration JSON, persist, and reload.
pub(crate) fn set_game_config(pkg: &str, config_json: &str) -> bool {
    let pkg = pkg.trim();
    if !is_valid_package(pkg) {
        warn!(comp::APPLIST, "set_game_config: invalid package '{pkg}'");
        return false;
    }
    let Some(parsed) = parse_flat_json(config_json) else {
        warn!(comp::APPLIST, "set_game_config: invalid JSON for '{pkg}'");
        return false;
    };

    let mut list = st().list.clone();
    list.insert(pkg.to_string());

    {
        let mut store = CONFIGS.lock().unwrap_or_else(|e| e.into_inner());
        ensure_configs_loaded(&mut store);
        let mut merged = store
            .entries
            .get(pkg)
            .cloned()
            .unwrap_or_else(default_entry);
        for (k, v) in parsed {
            if let Some(slot) = merged.get_mut(k.as_str()) {
                let clean = v.trim();
                if !clean.is_empty()
                    && clean.len() <= 64
                    && clean.chars().all(|c| !c.is_control())
                {
                    *slot = clean.to_string();
                }
            }
        }
        if merged.get("resolution_downscale").map(String::as_str) == Some("default") {
            merged.insert("resolution_fps".to_string(), "60".to_string());
        }
        store.entries.insert(pkg.to_string(), merged);
        if !persist_configs_locked(&store.entries) {
            return false;
        }
    }

    if !write_list(&list) {
        return false;
    }
    reload_list();
    true
}

/// Return the full managed game configuration map as JSON, synchronized with gamelist.txt.
pub(crate) fn game_configs_json() -> String {
    let list = st().list.clone();
    let mut store = CONFIGS.lock().unwrap_or_else(|e| e.into_inner());
    ensure_configs_loaded(&mut store);

    let mut merged: GameConfigMap = BTreeMap::new();
    for pkg in &list {
        let cfg = store
            .entries
            .get(pkg)
            .cloned()
            .unwrap_or_else(default_entry);
        merged.insert(pkg.clone(), cfg);
    }
    serialize_nested_json(&merged)
}

/// Re-scan installed CATEGORY_GAME packages and merge newly detected games into the list.
pub(crate) fn rescan_games() -> bool {
    let Some(detected) = crate::daemon::apps::installed_games() else {
        warn!(comp::APPLIST, "rescan_games: installed-game scan unavailable");
        return false;
    };

    let mut list = st().list.clone();
    let before = list.len();
    for pkg in detected {
        if is_valid_package(&pkg) {
            list.insert(pkg);
        }
    }

    {
        let mut store = CONFIGS.lock().unwrap_or_else(|e| e.into_inner());
        ensure_configs_loaded(&mut store);
        for pkg in &list {
            store.entries.entry(pkg.clone()).or_insert_with(default_entry);
        }
        let _ = persist_configs_locked(&store.entries);
    }

    if !write_list(&list) {
        return false;
    }
    let _ = fs::write(loaded_flag(), format!("{LOADED_VERSION} {}\n", list.len()));
    reload_list();
    info!(
        comp::APPLIST,
        "rescan_games complete: {} -> {} packages",
        before,
        list.len()
    );
    true
}


/// Return the stored per-game config map for a single package, if present.
pub(crate) fn get_game_config(pkg: &str) -> Option<BTreeMap<String, String>> {
    let mut store = CONFIGS.lock().unwrap_or_else(|e| e.into_inner());
    ensure_configs_loaded(&mut store);
    store.entries.get(pkg).cloned()
}

pub(crate) fn games() -> Vec<String> {
    st().list.iter().cloned().collect()
}

pub(crate) fn reload_list() {
    {
        let mut store = CONFIGS.lock().unwrap_or_else(|e| e.into_inner());
        ensure_configs_loaded(&mut store);
    }

    let text = match fs::read_to_string(LIST_FILE) {
        Ok(t) => t,
        Err(e) => {
            warn!(comp::APPLIST, "gamelist unreadable ({LIST_FILE}): {e}");
            return;
        }
    };
    let new = parse_list(&text);
    let total = new.len();
    let (added, removed, dropped) = {
        let mut g = st();
        let s = &mut *g;
        let added: Vec<String> = new.difference(&s.list).cloned().collect();
        let removed: Vec<String> = s.list.difference(&new).cloned().collect();
        s.list = new;
        let mut dropped = None;
        if matches!(&s.active, Some((p, _)) if !s.list.contains(p)) {
            dropped = s.active.take().map(|(p, _)| p);
        }
        (added, removed, dropped)
    };
    info!(comp::APPLIST, "gamelist loaded: {total} packages (+{} -{})", added.len(), removed.len());
    if !added.is_empty() { debug!(comp::APPLIST, "gamelist added: {}", added.join(", ")); }
    if !removed.is_empty() { debug!(comp::APPLIST, "gamelist removed: {}", removed.join(", ")); }
    if let Some(p) = dropped { info!(comp::APPLIST, "active app {p} removed from gamelist, falling back"); }
    debug!(comp::APPLIST, "full gamelist: {}", st().list.iter().cloned().collect::<Vec<_>>().join(", "));
    adopt("gamelist changed");
}

pub(crate) fn write_list(packages: &BTreeSet<String>) -> bool {
    let mut body = String::new();
    for pkg in packages {
        body.push_str(pkg);
        body.push('\n');
    }
    match fs::write(LIST_FILE, body) {
        Ok(()) => true,
        Err(e) => {
            error!(comp::APPLIST, "failed to write {LIST_FILE}: {e}");
            false
        }
    }
}

pub(crate) fn init_gamelist() {
    let upgrading = Path::new(&loaded_flag()).exists();
    if !upgrading || !flag_is_current() {
        let games = match crate::daemon::apps::installed_games() {
            Some(g) => g,
            None => {
                warn!(comp::APPLIST, "installed-game scan unavailable, will retry on the next boot");
                return;
            }
        };
        info!(
            comp::APPLIST,
            "{}: installed-game scan found {} games",
            if upgrading { "gamelist upgrade" } else { "first boot" },
            games.len()
        );

        let packages = if upgrading {
            match fs::read_to_string(LIST_FILE) {
                Ok(text) => parse_list(&text).intersection(&games).cloned().collect(),
                Err(e) => {
                    warn!(comp::APPLIST, "cannot read {LIST_FILE} for upgrade ({e}), rebuilding");
                    games
                }
            }
        } else {
            games
        };

        if !write_list(&packages) {
            return;
        }
        let _ = fs::write(loaded_flag(), format!("{LOADED_VERSION} {}\n", packages.len()));

        reload_list();
        info!(comp::APPLIST, "gamelist ready: {} games in {LIST_DIR}/{LIST_NAME}", packages.len());
        return;
    }

    debug!(comp::APPLIST, "gamelist already loaded ({}), skipping scan", loaded_flag());
}

fn flag_is_current() -> bool {
    fs::read_to_string(loaded_flag())
        .map(|text| text.trim_start().starts_with(LOADED_VERSION))
        .unwrap_or(false)
}

fn loaded_flag() -> String {
    format!("{LIST_DIR}/{LOADED_FLAG}")
}

pub(crate) fn watch_list() {
    thread::spawn(|| unsafe {
        let fd = libc::inotify_init1(libc::IN_CLOEXEC);
        if fd < 0 { error!(comp::APPLIST, "inotify_init1 failed"); return; }
        let dir = std::ffi::CString::new(LIST_DIR).unwrap();
        let wd = libc::inotify_add_watch(fd, dir.as_ptr(), libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO);
        if wd < 0 { error!(comp::APPLIST, "inotify_add_watch failed: {LIST_DIR}"); return; }
        info!(comp::APPLIST, "watching for changes to {LIST_DIR}/{LIST_NAME}");

        let mut buf = [0u8; 4096];
        loop {
            let r = libc::read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
            if r < 0 && *libc::__errno() == libc::EINTR { continue; }
            if r <= 0 { error!(comp::APPLIST, "inotify read stopped (r={r})"); return; }
            let total = r as usize;
            let (mut off, mut hit) = (0usize, false);
            while off + 16 <= total {
                let len = u32::from_ne_bytes(buf[off + 12..off + 16].try_into().unwrap()) as usize;
                if off + 16 + len > total { break; }
                let name = buf[off + 16..off + 16 + len].split(|&b| b == 0).next().unwrap_or(&[]);
                if name == LIST_NAME.as_bytes() { hit = true; }
                off += 16 + len;
            }
            if hit {
                debug!(comp::APPLIST, "inotify: {LIST_NAME} changed");
                reload_list();
            }
        }
    });
}

// ---------------- minimal zero-dependency JSON parser / serializer ----------------

struct JsonCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonCursor<'a> {
    fn new(s: &'a str) -> Self {
        Self { bytes: s.as_bytes(), pos: 0 }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.bytes.get(self.pos).copied()
    }

    fn consume(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_string(&mut self) -> Option<String> {
        if !self.consume(b'"') {
            return None;
        }
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            self.pos += 1;
            match b {
                b'"' => return Some(out),
                b'\\' => {
                    if self.pos >= self.bytes.len() {
                        return None;
                    }
                    let esc = self.bytes[self.pos];
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'u' => {
                            if self.pos + 4 > self.bytes.len() {
                                return None;
                            }
                            let hex = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4]).ok()?;
                            self.pos += 4;
                            let code = u32::from_str_radix(hex, 16).ok()?;
                            if let Some(ch) = char::from_u32(code) {
                                out.push(ch);
                            }
                        }
                        _ => return None,
                    }
                }
                _ => out.push(b as char),
            }
        }
        None
    }

    fn parse_scalar_as_string(&mut self) -> Option<String> {
        match self.peek()? {
            b'"' => self.parse_string(),
            _ => {
                let start = self.pos;
                while self.pos < self.bytes.len() {
                    let b = self.bytes[self.pos];
                    if b == b',' || b == b'}' || b.is_ascii_whitespace() {
                        break;
                    }
                    self.pos += 1;
                }
                if self.pos == start {
                    return None;
                }
                std::str::from_utf8(&self.bytes[start..self.pos])
                    .ok()
                    .map(|s| s.to_string())
            }
        }
    }

    fn parse_flat_object(&mut self) -> Option<BTreeMap<String, String>> {
        if !self.consume(b'{') {
            return None;
        }
        let mut map = BTreeMap::new();
        if self.consume(b'}') {
            return Some(map);
        }
        loop {
            let key = self.parse_string()?;
            if !self.consume(b':') {
                return None;
            }
            let val = self.parse_scalar_as_string()?;
            map.insert(key, val);
            if self.consume(b',') {
                continue;
            }
            if self.consume(b'}') {
                return Some(map);
            }
            return None;
        }
    }

    fn parse_nested_object(&mut self) -> Option<GameConfigMap> {
        if !self.consume(b'{') {
            return None;
        }
        let mut map = BTreeMap::new();
        if self.consume(b'}') {
            return Some(map);
        }
        loop {
            let key = self.parse_string()?;
            if !self.consume(b':') {
                return None;
            }
            let inner = self.parse_flat_object()?;
            map.insert(key, inner);
            if self.consume(b',') {
                continue;
            }
            if self.consume(b'}') {
                return Some(map);
            }
            return None;
        }
    }
}

fn parse_flat_json(s: &str) -> Option<BTreeMap<String, String>> {
    let mut cur = JsonCursor::new(s);
    let obj = cur.parse_flat_object()?;
    if cur.peek().is_none() { Some(obj) } else { None }
}

fn parse_nested_json(s: &str) -> Option<GameConfigMap> {
    let mut cur = JsonCursor::new(s);
    let obj = cur.parse_nested_object()?;
    if cur.peek().is_none() { Some(obj) } else { None }
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn serialize_nested_json(map: &GameConfigMap) -> String {
    let mut out = String::from("{\n");
    for (i, (pkg, cfg)) in map.iter().enumerate() {
        out.push_str("  \"");
        out.push_str(&escape_json(pkg));
        out.push_str("\": {\n");
        for (j, (k, v)) in cfg.iter().enumerate() {
            out.push_str("    \"");
            out.push_str(&escape_json(k));
            out.push_str("\": \"");
            out.push_str(&escape_json(v));
            out.push('"');
            if j + 1 < cfg.len() {
                out.push(',');
            }
            out.push('\n');
        }
        out.push_str("  }");
        if i + 1 < map.len() {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    out
}
