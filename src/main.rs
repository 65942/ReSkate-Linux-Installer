mod gui;

use anyhow::{Context, Result, bail};
use keyvalues_parser::{Obj, Value};
use sha2::Digest;
use std::borrow::Cow;
use std::{fs, path::{Path, PathBuf}, process::Command};

/// skate. on Steam.
pub const SKATE_APPID: u32 = 3354750;
const REPO: &str = "Dingo-Shenanigans/ReSkate";
const LAUNCHER: &str = "ReSkateLauncher.exe";
/// What Steam starts for skate.; EA's anti-cheat refuses to run under Proton, so the launch option swaps it.
const ANTICHEAT: &str = "EAAntiCheat.GameServiceLauncher.exe";
/// Forwards Discord Rich Presence from the game's Wine prefix to Discord (or Vesktop) on Linux.
const BRIDGE_REPO: &str = "EnderIce2/rpc-bridge";
/// rpc-bridge's folder inside ReSkate's, and its Steam script there.
const BRIDGE_DIR: &str = "rpc-bridge";
const BRIDGE_SH: &str = "rpc-bridge/bridge.sh";
/// The desktop's file picker in place of Wine's (`--native-picker`): our DLL in skate.'s prefix asks this
/// installer's `picker-helper`, copied next to it and wrapping the game in the launch option.
const PICKER_DIR: &str = "native-picker";
const PICKER_DLL: &str = "native-picker/nativepicker.dll";
const PICKER_BIN: &str = "native-picker/reskate-installer";
static NATIVE_PICKER_DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/nativepicker.dll"));
/// Where Wine's COM finds the Open dialog (only HKLM; it ignores HKCU's classes). 64-bit only, like ReSkate.
const FILE_DIALOG_KEY: &str = r"[Software\\Classes\\CLSID\\{DC1C5A9C-E88A-4DDE-A5A1-60F82A20AEF7}\\InprocServer32]";
const WINE_DIALOG: &str = r"C:\windows\system32\comdlg32.dll";
/// Opening a folder (ReSkate's "Open Mods folder") goes to Wine's own explorer without this; winebrowser hands it to
/// the desktop's file manager through xdg-open. Wine's default prefix doesn't have the key.
const FOLDER_OPEN_KEY: &str = r"[Software\\Classes\\folder\\shell\\open\\command]";
const FOLDER_OPEN: &str = r#"@="C:\\windows\\system32\\winebrowser.exe \"%1\"""#;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let cmd = args.first().map(String::as_str);
    if cmd.is_none() { return gui::run().map_err(|e| anyhow::anyhow!("{e}")); }
    // picker-helper <command...>: run from the launch option, everything after it is the game's.
    if cmd == Some("picker-helper") { return picker_helper(&args[1..]); }
    // --close-steam: quit Steam for the edit (the GUI asks first) and start it again afterwards.
    let closed = flag("--close-steam") && steam_running() && { close_steam()?; true };
    let result = match cmd {
        // install [<folder>] [--version <tag>] [--rpc] [--native-picker] [--fix-names] [--close-steam]
        Some("install") => {
            let version = args.iter().position(|a| a == "--version").and_then(|i| args.get(i + 1)).map(String::as_str);
            let dir = args.iter().skip(1).find(|a| !a.starts_with("--") && Some(a.as_str()) != version).map(PathBuf::from);
            install(dir, version, flag("--rpc"), flag("--native-picker"), flag("--fix-names"))
        }
        // off [--delete] [--close-steam]
        Some("off") => uninstall(flag("--delete")),
        _ => Err(anyhow::anyhow!("unknown command, use: install [<folder>] [--version <tag>] [--rpc] [--native-picker] [--fix-names] | off [--delete], either with --close-steam")),
    };
    if closed {
        println!("starting Steam again");
        spawn_detached(steam_command());
    }
    result
}

/// Downloads a ReSkate release (the latest unless `version`) into `dir` and points skate.'s Steam launch option at
/// its launcher. The launcher keeps itself up to date from then on, unless a version was picked: that one is pinned.
/// `rpc` adds rpc-bridge to the launch option, for Discord Rich Presence. `picker` sets up the desktop's file picker
/// for ReSkate's Open dialogs. `fix_names` runs `fix_names` on the folder.
fn install(dir: Option<PathBuf>, version: Option<&str>, rpc: bool, picker: bool, fix_names: bool) -> Result<()> {
    if let Some(v) = version && !supported(v) { bail!("{v} is too old: ReSkate installs into a folder of its own from {OLDEST} on"); }
    if steam_running() { bail!("Close Steam first, or pass --close-steam (Steam rewrites its settings when it closes)"); }
    // The Steam library to write skate.'s placeholder into: a new one, or the one there (to its current build).
    let placeholder_lib = match (skate_dir()?, placeholder()) {
        (_, Some(d)) => Some(d.parent().and_then(Path::parent).context("odd skate. folder")?.to_owned()),
        (None, _) => Some(steam()?.path().join("steamapps")),
        _ => None,
    };
    let record = read_record();
    let dir = dir.or_else(|| record.as_ref().map(|r| r.dir.clone())).unwrap_or_else(default_dir);
    if let Some(r) = &record && r.dir != dir {
        bail!("ReSkate is already installed in {}: uninstall it first", r.dir.display());
    }
    check_dir(&dir)?;
    // Proton splits the bridge's command (bridge.sh's PROTON_REMOTE_DEBUG_CMD) on spaces.
    if rpc && dir.to_string_lossy().contains(char::is_whitespace) { bail!("Discord Rich Presence needs a folder without spaces in its path"); }
    let release = github_json(&match version { Some(v) => format!("{REPO}/releases/tags/{v}"), None => format!("{REPO}/releases/latest") })
        .with_context(|| format!("release {} not found", version.unwrap_or("latest")))?;
    let tag = release["tag_name"].as_str().context("release has no tag")?;
    let asset = release["assets"].as_array().into_iter().flatten()
        .filter_map(|a| a["name"].as_str())
        .find(|n| n.starts_with("ReSkate-") && n.ends_with(".zip"))
        .with_context(|| format!("release {tag} has no ReSkate zip"))?;
    println!("downloading {asset}");
    let mut files = extract(&github_asset(&release, asset)?, &dir)?;
    if !files.iter().any(|f| f == LAUNCHER) { bail!("the release zip has no {LAUNCHER}"); }
    // A workaround, so it doesn't fail the install.
    if fix_names {
        match self::fix_names(&dir, true) {
            Ok(0) => {}
            Ok(n) => println!("fixed {n} file name(s) with `\\` in them"),
            Err(e) => eprintln!("couldn't fix the file names with `\\` in them: {e}"),
        }
    }
    if rpc {
        let bridge = github_json(&format!("{BRIDGE_REPO}/releases/latest"))?;
        println!("downloading rpc-bridge {}", bridge["tag_name"].as_str().unwrap_or("?"));
        let got = extract(&github_asset(&bridge, "bridge.zip")?, &dir.join(BRIDGE_DIR))?;
        if !got.iter().any(|f| f == "bridge.sh") { bail!("rpc-bridge's zip has no bridge.sh"); }
        files.extend(got.iter().map(|f| format!("{BRIDGE_DIR}/{f}")));
        if flatpak_steam() {
            println!("Flatpak Steam can't reach Discord on its own: run `flatpak override --user --filesystem=xdg-run/discord-ipc-0 com.valvesoftware.Steam` once");
        }
    } else if dir.join(BRIDGE_DIR).exists() { fs::remove_dir_all(dir.join(BRIDGE_DIR))?; }
    if picker {
        fs::create_dir_all(dir.join(PICKER_DIR))?;
        fs::write(dir.join(PICKER_DLL), NATIVE_PICKER_DLL)?;
        // A copy of this binary, so the launch option keeps working when the AppImage moves.
        let (exe, bin) = (std::env::current_exe()?, dir.join(PICKER_BIN));
        if exe != bin {
            let tmp = bin.with_extension("new");
            fs::copy(&exe, &tmp)?;
            fs::rename(&tmp, &bin)?;
        }
        files.extend([PICKER_DLL, PICKER_BIN].map(str::to_owned));
    } else if dir.join(PICKER_DIR).exists() { fs::remove_dir_all(dir.join(PICKER_DIR))?; }
    // Kept from before the first install, so uninstalling puts the user's own options back.
    let before = match &record {
        Some(r) => r.before.clone(),
        None => launch_options()?.filter(|o| !o.contains(ANTICHEAT)).unwrap_or_default(),
    };
    let pinned = version.map_or(String::new(), |_| tag.to_owned());
    if let Some(lib) = &placeholder_lib { write_placeholder(lib)?; }
    set_launch_options(&launch_option(&dir, !pinned.is_empty(), rpc, picker))?;
    // Steam is closed, so skate.'s Wine isn't running and won't overwrite this. Without a prefix yet,
    // picker-helper sets it on the first start.
    if let Some(pfx) = skate_prefix().filter(|p| p.exists()) {
        let dll = picker.then(|| dir.join(PICKER_DLL));
        if let Err(e) = set_prefix_dialog(&pfx, dll.as_deref()) { eprintln!("couldn't set skate.'s file dialog: {e}"); }
    }
    write_record(&Record { dir: dir.clone(), before, pinned: pinned.clone(), files })?;
    println!("ReSkate {tag} installed in {}", dir.display());
    if !pinned.is_empty() { println!("Pinned to {tag}: the launcher won't update itself. Install the latest version to follow updates again."); }
    println!("Press Play on skate. in Steam: the ReSkate launcher opens and installs its own copy of the game there (about 14 GB) the first time.");
    Ok(())
}

/// Puts skate.'s launch option back and removes ReSkate's files, or with `delete` its whole folder
/// (its copy of the game, mods and settings).
fn uninstall(delete: bool) -> Result<()> {
    if steam_running() { bail!("Close Steam first, or pass --close-steam (Steam rewrites its settings when it closes)"); }
    let r = read_record().context("ReSkate isn't installed")?;
    // Left alone if the user changed it since.
    if launch_options()?.as_deref() == Some(&launch_option(&r.dir, !r.pinned.is_empty(), r.rpc(), r.picker())) { set_launch_options(&r.before)?; }
    if r.picker() && let Some(pfx) = skate_prefix().filter(|p| p.exists()) {
        if let Err(e) = set_prefix_dialog(&pfx, None) { eprintln!("couldn't put back Wine's file dialog: {e}"); }
    }
    // Without ReSkate, Play would start EA's anti-cheat on a game that isn't there.
    if let Some(d) = placeholder() {
        fs::remove_file(d.parent().and_then(Path::parent).context("odd skate. folder")?.join(format!("appmanifest_{SKATE_APPID}.acf")))?;
        fs::remove_dir_all(&d)?;
        println!("removed skate.'s placeholder from Steam");
    }
    if delete {
        if r.dir.exists() { fs::remove_dir_all(&r.dir)?; }
    } else {
        for f in &r.files { let _ = fs::remove_file(r.dir.join(f)); }
        // Emptied folders (licenses/, and the folder itself if nothing else is left); non-empty ones just fail.
        let mut dirs: Vec<PathBuf> = r.files.iter().filter_map(|f| Some(r.dir.join(Path::new(f).parent()?))).collect();
        dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
        for d in dirs { let _ = fs::remove_dir(d); }
    }
    fs::remove_file(data_dir()?.join("install.txt"))?;
    println!("ReSkate uninstalled{}", if delete { format!(", deleted {}", r.dir.display()) } else { String::new() });
    Ok(())
}

/// Unpacks a release zip into `dir`; returns the files written, relative to it.
fn extract(zip: &[u8], dir: &Path) -> Result<Vec<String>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(zip))?;
    let mut files = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        // Their release zips are made on Windows, with `\` separators.
        let name = entry.name()?.replace('\\', "/");
        let rel = Path::new(&name);
        if name.ends_with('/') || !rel.components().all(|c| matches!(c, std::path::Component::Normal(_))) { continue; }
        let dst = dir.join(rel);
        fs::create_dir_all(dst.parent().unwrap())?;
        std::io::copy(&mut entry, &mut fs::File::create(&dst)?)?;
        files.push(name);
    }
    Ok(files)
}

/// Files a Linux unzip tool left with `\` in their names (ReSkate's release zips use `\` separators, and some mods'
/// may): under Proton they throw off the paths in ReSkate's folder listings, so it can't load the game's Oodle DLL and
/// its mod merge keeps one mod's copy of shared files (ReSkate issues 109 and 124). Moves each where it belongs, or
/// deletes it when that file is already there; returns how many it fixed (without `apply`: would fix, changing nothing).
// ponytail: workaround; delete once the oldest supported release has ReSkate's fix (commit d0985ff, from v2.0.8)
fn fix_names(dir: &Path, apply: bool) -> Result<usize> {
    let mut fixed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let (path, is_dir) = (entry.path(), entry.file_type()?.is_dir());
        if is_dir { fixed += fix_names(&path, apply)?; }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.contains('\\') { continue; }
        // A folder entry (`licenses\`): fixed when empty.
        if is_dir {
            if fs::read_dir(&path)?.next().is_none() { if apply { fs::remove_dir(&path)?; } fixed += 1; }
            continue;
        }
        let name = name.replace('\\', "/");
        let rel = Path::new(&name);
        if !rel.components().all(|c| matches!(c, std::path::Component::Normal(_))) { continue; }
        if apply {
            let dst = dir.join(rel);
            if dst.exists() { fs::remove_file(&path)?; } else { fs::create_dir_all(dst.parent().unwrap())?; fs::rename(&path, &dst)?; }
        }
        fixed += 1;
    }
    Ok(fixed)
}

/// Whether ReSkate's folder has file names `fix_names` would fix.
pub fn broken_names(dir: &Path) -> bool { fix_names(dir, false).is_ok_and(|n| n > 0) }

/// The Steam launch option that runs ReSkate's launcher in place of EA's anti-cheat (ReSkate's README,
/// "Linux (Proton)"). Keeps skate.'s app ID and Proton prefix, unlike a non-Steam game shortcut. A pinned version
/// gets the launcher's `--no-update`, or it would update itself to the latest on its first start. With `rpc`,
/// rpc-bridge's script wraps it all and has Proton start the bridge next to the game. With `picker`, picker-helper
/// wraps the rest and answers the native picker DLL while the game runs.
pub fn launch_option(dir: &Path, pinned: bool, rpc: bool, picker: bool) -> String {
    let no_update = if pinned { " --no-update" } else { "" };
    let bridge = if rpc { format!("sh \"{}\" ", dir.join(BRIDGE_SH).display()) } else { String::new() };
    let helper = if picker { format!("\"{}\" picker-helper ", dir.join(PICKER_BIN).display()) } else { String::new() };
    format!("{bridge}{helper}bash -c 'exec \"${{@/*{ANTICHEAT}/{}}}\"{no_update}' -- %command%", dir.join(LAUNCHER).display())
}

/// The first release whose launcher installs the game into an empty folder of its own (and has `--no-update`).
const OLDEST: &str = "v1.0.1";

/// Whether a release tag is `OLDEST` or newer.
fn supported(tag: &str) -> bool {
    let v = |t: &str| t.trim_start_matches('v').split('.').map(|n| n.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    v(tag) >= v(OLDEST)
}

/// Releases this installer can install, newest first, and whether each is a pre-release. A plain install follows
/// GitHub's releases/latest, which skips those.
pub fn releases() -> Result<Vec<(String, bool)>> {
    let list = github_json(&format!("{REPO}/releases?per_page=100"))?;
    Ok(list.as_array().into_iter().flatten().filter(|r| r["draft"] != true)
        .filter_map(|r| Some((r["tag_name"].as_str()?.to_owned(), r["prerelease"] == true))).filter(|(t, _)| supported(t)).collect())
}

/// A folder the launcher can run from, and that fits into the launch option as is.
pub fn check_dir(dir: &Path) -> Result<()> {
    if !dir.is_absolute() { bail!("use a full path"); }
    let s = dir.to_string_lossy();
    // Quotes and shell syntax would break the launch option; `&` is special in bash's ${@/...} replacement.
    if let Some(c) = s.chars().find(|c| "'\"\\$`&}\n".contains(*c)) { bail!("the path can't contain {c:?}"); }
    // The launcher refuses to run from anywhere in a Steam library's games (it checks for steamapps/common).
    let names: Vec<String> = dir.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect();
    if names.windows(2).any(|w| w[0] == "steamapps" && w[1] == "common") {
        bail!("ReSkate can't run from inside a Steam library's steamapps/common: pick a folder of its own");
    }
    // A new, empty folder, or one that already has ReSkate or its copy of the game (a reinstall, one kept
    // by an uninstall, or set up by hand before).
    let empty = fs::read_dir(dir).map_or(true, |mut d| d.next().is_none());
    if !empty && ![LAUNCHER, "Skate.exe"].iter().any(|f| dir.join(f).exists()) { bail!("pick a new, empty folder: ReSkate installs its own copy of the game there"); }
    Ok(())
}

/// `~/Games/ReSkate`, the folder ReSkate's README suggests. Flatpak Steam can't see the rest of the home
/// folder, so there it goes inside Steam's own.
pub fn default_dir() -> PathBuf {
    let home = dirs::home_dir().unwrap_or_default();
    if flatpak_steam() { home.join(".var/app/com.valvesoftware.Steam/Games/ReSkate") } else { home.join("Games/ReSkate") }
}

/// What the installer did: ReSkate's folder, skate.'s launch options from before, the version it's pinned to
/// ("" = the latest, updating itself), and the files it unpacked.
struct Record { dir: PathBuf, before: String, pinned: String, files: Vec<String> }

impl Record {
    /// Installed with rpc-bridge.
    fn rpc(&self) -> bool { self.files.iter().any(|f| f == BRIDGE_SH) }
    /// Installed with the native file picker.
    fn picker(&self) -> bool { self.files.iter().any(|f| f == PICKER_BIN) }
}

fn read_record() -> Option<Record> {
    let text = fs::read_to_string(data_dir().ok()?.join("install.txt")).ok()?;
    let mut lines = text.lines();
    Some(Record { dir: lines.next()?.into(), before: lines.next()?.to_owned(), pinned: lines.next()?.to_owned(), files: lines.map(str::to_owned).collect() })
}

fn write_record(r: &Record) -> Result<()> {
    fs::create_dir_all(data_dir()?)?;
    Ok(fs::write(data_dir()?.join("install.txt"), format!("{}\n{}\n{}\n{}\n", r.dir.display(), r.before, r.pinned, r.files.join("\n")))?)
}

/// ReSkate's folder, when installed.
pub fn installed() -> Option<PathBuf> {
    read_record().map(|r| r.dir).filter(|d| d.join(LAUNCHER).exists())
}

/// Whether skate.'s launch option still starts ReSkate (Steam or the user may have changed it).
pub fn launch_ok() -> bool {
    let Some(r) = read_record().filter(|r| r.dir.join(LAUNCHER).exists()) else { return false };
    launch_options().ok().flatten() == Some(launch_option(&r.dir, !r.pinned.is_empty(), r.rpc(), r.picker()))
}

/// Whether ReSkate was installed with Discord Rich Presence (rpc-bridge).
pub fn rpc() -> bool {
    read_record().is_some_and(|r| r.rpc())
}

/// Whether ReSkate was installed with the native file picker.
pub fn picker() -> bool {
    read_record().is_some_and(|r| r.picker())
}

/// The version ReSkate is pinned to, if one was picked.
pub fn pinned() -> Option<String> {
    read_record().map(|r| r.pinned).filter(|p| !p.is_empty())
}

/// The Steam install to use. Several can coexist (native, Flatpak, Snap, Debian's), so prefer the one that has
/// skate.
fn steam() -> Result<steamlocate::SteamDir> {
    static CHOSEN: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    let chosen = CHOSEN.get_or_init(|| {
        let home = dirs::home_dir()?;
        let data = dirs::data_dir()?;
        // Same places steamlocate checks on Linux; it only returns the first one found.
        let candidates = [
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
            home.join(".var/app/com.valvesoftware.Steam/.steam/steam"),
            data.join("Steam"),
            home.join(".steam/steam"),
            home.join(".steam/debian-installation"),
            home.join("snap/steam/common/.local/share/Steam"),
            home.join("snap/steam/common/.steam/steam"),
        ];
        candidates.into_iter()
            .find(|p| steamlocate::SteamDir::from_dir(p).is_ok_and(|s| s.find_app(SKATE_APPID).ok().flatten().is_some()))
    });
    Ok(match chosen {
        Some(p) => steamlocate::SteamDir::from_dir(p)?,
        None => steamlocate::SteamDir::locate()?,
    })
}

/// skate.'s Steam install folder, when installed (for real or as a placeholder): Play in Steam is what starts ReSkate.
pub fn skate_dir() -> Result<Option<PathBuf>> {
    Ok(steam()?.find_app(SKATE_APPID)?.map(|(app, lib)| lib.resolve_app_dir(&app)))
}

/// skate.'s depot with the game itself; the other one is EA's anti-cheat.
const GAME_DEPOT: &str = "3354751";

/// skate.'s folder when it's only a placeholder: Steam has it as fully installed (StateFlags bit 4), but without
/// Skate.exe. A real install that Steam is still downloading isn't one.
pub fn placeholder() -> Option<PathBuf> {
    let dir = skate_dir().ok()??;
    let acf = fs::read_to_string(dir.parent()?.parent()?.join(format!("appmanifest_{SKATE_APPID}.acf"))).ok()?;
    let flags: u32 = vdf_get(&acf, &["StateFlags"])?.parse().ok()?;
    (flags & 4 != 0 && !dir.join("Skate.exe").exists()).then_some(dir)
}

/// Makes Steam see skate. as installed without its 15 GB copy, which is never played (the launch option starts
/// ReSkate's own copy). It claims Steam's current build, so Steam downloads only the anti-cheat depot (~350 MB),
/// which has the file the launch option swaps out. Run again once skate. updates on Steam, or Steam downloads it all.
fn write_placeholder(steamapps: &Path) -> Result<()> {
    let (build, manifest) = steam_build()?;
    fs::create_dir_all(steamapps.join("common/Skate"))?;
    let acf = steamapps.join(format!("appmanifest_{SKATE_APPID}.acf"));
    // An existing placeholder keeps its anti-cheat depot, or Steam would fetch it again.
    let text = fs::read_to_string(&acf).unwrap_or_else(|_| "\"AppState\"\n{\n}\n".to_owned());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
    fs::write(&acf, placeholder_manifest(&text, &build, &manifest, steam_user()?, now)?)?;
    println!("skate.'s placeholder in Steam is at build {build}. If new, Steam downloads EA's anti-cheat (~350 MB) for it");
    Ok(())
}

/// AutoUpdateBehavior 1: only update when launched, so a skate. update waits for Repair instead of downloading 15 GB.
fn placeholder_manifest(text: &str, build: &str, manifest: &str, owner: u64, now: u64) -> Result<String> {
    vdf_edit(text, |a| {
        // What's left of an update Steam had started, which it would otherwise carry on with.
        a.retain(|k, _| !["TargetBuildID", "StagedDepots", "StagingSize", "BytesToDownload", "BytesDownloaded", "BytesToStage",
            "BytesStaged", "UpdateResult"].iter().any(|s| k.eq_ignore_ascii_case(s)));
        let fields = [("appid", SKATE_APPID.to_string()), ("universe", "1".into()), ("name", "skate.".into()), ("StateFlags", "4".into()),
            ("installdir", "Skate".into()), ("LastUpdated", now.to_string()), ("buildid", build.into()), ("LastOwner", owner.to_string()),
            ("AutoUpdateBehavior", "1".into())];
        for (k, v) in fields { set(a, k, &v); }
        set(child(child(a, "InstalledDepots"), GAME_DEPOT), "manifest", manifest);
    })
}

/// Whether skate. was updated on Steam since its placeholder was written.
pub fn placeholder_outdated() -> bool {
    let Some(dir) = placeholder() else { return false };
    let acf = dir.parent().and_then(Path::parent).and_then(|s| fs::read_to_string(s.join(format!("appmanifest_{SKATE_APPID}.acf"))).ok());
    matches!((acf.and_then(|a| vdf_get(&a, &["buildid"])), steam_build()), (Some(a), Ok((b, _))) if a != b)
}

/// skate.'s current build and game depot manifest, from Steam's app info cache.
fn steam_build() -> Result<(String, String)> {
    let path = steam()?.path().join("appcache/appinfo.vdf");
    appinfo_build(&fs::read(&path).with_context(|| format!("reading {}", path.display()))?)
}

/// A binary KeyValues tree, as in appinfo.vdf.
enum Kv { Map(Vec<(String, Kv)>), Val(String) }

impl Kv {
    fn get(&self, path: &[&str]) -> Option<&str> {
        match (self, path) {
            (Kv::Val(v), []) => Some(v),
            (Kv::Map(m), [k, rest @ ..]) => m.iter().find(|(n, _)| n.eq_ignore_ascii_case(k))?.1.get(rest),
            _ => None,
        }
    }
}

fn take<'a>(d: &mut &'a [u8], n: usize) -> Result<&'a [u8]> {
    if d.len() < n { bail!("appinfo.vdf is cut short"); }
    let (a, b) = d.split_at(n);
    *d = b;
    Ok(a)
}

fn take_u32(d: &mut &[u8]) -> Result<u32> { Ok(u32::from_le_bytes(take(d, 4)?.try_into()?)) }

fn take_str(d: &mut &[u8]) -> Result<String> {
    let n = d.iter().position(|&b| b == 0).context("appinfo.vdf is cut short")?;
    Ok(String::from_utf8_lossy(&take(d, n + 1)?[..n]).into_owned())
}

/// A map's entries up to its end byte. v29 files name keys by index into a string table.
fn binary_kv(d: &mut &[u8], strings: Option<&[String]>) -> Result<Kv> {
    let mut map = Vec::new();
    loop {
        let t = take(d, 1)?[0];
        if t == 8 { return Ok(Kv::Map(map)); }
        let key = match strings {
            Some(s) => s.get(take_u32(d)? as usize).context("appinfo.vdf: bad key")?.clone(),
            None => take_str(d)?,
        };
        let val = match t {
            0 => binary_kv(d, strings)?,
            1 => Kv::Val(take_str(d)?),
            2 => Kv::Val(i32::from_le_bytes(take(d, 4)?.try_into()?).to_string()),
            3 => Kv::Val(f32::from_le_bytes(take(d, 4)?.try_into()?).to_string()),
            7 => Kv::Val(u64::from_le_bytes(take(d, 8)?.try_into()?).to_string()),
            10 => Kv::Val(i64::from_le_bytes(take(d, 8)?.try_into()?).to_string()),
            _ => bail!("appinfo.vdf: unknown value type {t}"),
        };
        map.push((key, val));
    }
}

/// skate.'s public build ID and game depot manifest from appinfo.vdf (format v28 or v29).
fn appinfo_build(data: &[u8]) -> Result<(String, String)> {
    let mut d = data;
    let magic = take_u32(&mut d)?;
    take_u32(&mut d)?; // universe
    let strings = match magic {
        0x0756_4429 => {
            let at = u64::from_le_bytes(take(&mut d, 8)?.try_into()?) as usize;
            let mut t = data.get(at..).context("appinfo.vdf is cut short")?;
            let n = take_u32(&mut t)?;
            Some((0..n).map(|_| take_str(&mut t)).collect::<Result<Vec<_>>>()?)
        }
        0x0756_4428 => None,
        _ => bail!("Steam's appinfo.vdf has an unknown format ({magic:#x})"),
    };
    loop {
        let app = take_u32(&mut d)?;
        if app == 0 { bail!("Steam has no info on skate. yet: start Steam once, then try again"); }
        let size = take_u32(&mut d)? as usize;
        let mut entry = take(&mut d, size)?;
        if app != SKATE_APPID { continue; }
        take(&mut entry, 60)?; // info state, last updated, PICS token, SHA-1, change number, binary SHA-1
        let kv = binary_kv(&mut entry, strings.as_deref())?;
        let get = |p: &[&str]| kv.get(&[&["appinfo", "depots"][..], p].concat()).map(str::to_owned);
        let build = get(&["branches", "public", "buildid"]).context("appinfo.vdf has no build for skate.")?;
        // Older entries have the manifest ID right under "public".
        let manifest = get(&[GAME_DEPOT, "manifests", "public", "gid"]).or_else(|| get(&[GAME_DEPOT, "manifests", "public"]))
            .context("appinfo.vdf has no manifest for skate.")?;
        return Ok((build, manifest));
    }
}

/// SteamID64 of the account last signed in (MostRecent, else latest Timestamp).
fn steam_user() -> Result<u64> {
    let text = fs::read_to_string(steam()?.path().join("config/loginusers.vdf")).context("no Steam account has signed in yet")?;
    let vdf = keyvalues_parser::parse(&text)?;
    let field = |v: &Value, k: &str| v.get_obj().and_then(|o| o.get(k)).and_then(|v| v[0].get_str()).unwrap_or("0").parse::<u64>().unwrap_or(0);
    let users = vdf.value.get_obj().context("loginusers.vdf has no users")?;
    let (id, _) = users.iter().max_by_key(|(_, v)| (field(&v[0], "MostRecent"), field(&v[0], "Timestamp")))
        .context("no Steam account has signed in yet")?;
    Ok(id.parse()?)
}

/// localconfig.vdf of the Steam account last signed in, which holds its launch options.
fn local_config() -> Result<PathBuf> {
    // userdata folders are named by the account ID, the low 32 bits of the SteamID64.
    Ok(steam()?.path().join(format!("userdata/{}/config/localconfig.vdf", steam_user()? & 0xffff_ffff)))
}

const LAUNCH_PATH: [&str; 6] = ["Software", "Valve", "Steam", "apps", "3354750", "LaunchOptions"];

/// skate.'s launch options (None when never set).
fn launch_options() -> Result<Option<String>> {
    let text = fs::read_to_string(local_config()?)?;
    Ok(vdf_get(&text, &LAUNCH_PATH))
}

fn set_launch_options(opts: &str) -> Result<()> {
    edit_steam_file(&local_config()?, |root| {
        let (keys, last) = LAUNCH_PATH.split_at(LAUNCH_PATH.len() - 1);
        let app = keys.iter().fold(root, |o, k| child(o, k));
        set(app, last[0], opts);
    })
}

/// Steam's own launcher: Flatpak's, or the one on PATH.
fn steam_command() -> Command {
    if flatpak_steam() {
        let mut c = Command::new("flatpak");
        c.args(["run", "com.valvesoftware.Steam"]);
        c
    } else { Command::new("steam") }
}

fn flatpak_steam() -> bool {
    let flatpak = dirs::home_dir().unwrap_or_default().join(".var/app/com.valvesoftware.Steam");
    steam().is_ok_and(|s| s.path().starts_with(flatpak))
}

/// Quits Steam the way its Exit menu does, then waits for it to save its settings and exit. Never killed:
/// that could lose the very config files we edit.
fn close_steam() -> Result<()> {
    println!("closing Steam");
    spawn_detached({ let mut c = steam_command(); c.arg("-shutdown"); c });
    let start = std::time::Instant::now();
    while steam_running() {
        if start.elapsed() > std::time::Duration::from_secs(60) { bail!("Steam didn't close: close it yourself, then try again"); }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}

/// Starts a program without waiting for it or sharing our output (the GUI reads that until every writer is gone),
/// in its own process group so it outlives us.
fn spawn_detached(mut cmd: Command) {
    use std::process::Stdio;
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    if let Err(e) = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        eprintln!("couldn't start {:?}: {e}", cmd.get_program());
    }
}

pub fn steam_running() -> bool {
    // pgrep prints PIDs on a match.
    Command::new("pgrep").args(["-x", "steam"]).output().is_ok_and(|o| !o.stdout.is_empty())
}

/// Steam rewrites its config files on exit, so edits made while it runs are lost.
fn edit_steam_file(path: &Path, edit: impl FnOnce(&mut Obj)) -> Result<()> {
    if steam_running() { bail!("Close Steam first, then try again"); }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    fs::write(path, vdf_edit(&text, edit)?)?;
    Ok(())
}

fn vdf_edit(text: &str, edit: impl FnOnce(&mut Obj)) -> Result<String> {
    let mut vdf = keyvalues_parser::parse(text)?;
    edit(vdf.value.get_mut_obj().context("vdf root is not an object")?);
    let mut out = String::new();
    write_vdf(&mut out, 0, &vdf.key, &vdf.value);
    Ok(out)
}

/// Steam escapes only `\` and `"` (newlines stay literal), which the crate's renderers don't match.
fn write_vdf(out: &mut String, depth: usize, key: &str, val: &Value) {
    let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    let tabs = "\t".repeat(depth);
    match val {
        Value::Str(s) => out.push_str(&format!("{tabs}{}\t\t{}\n", q(key), q(s))),
        Value::Obj(obj) => {
            out.push_str(&format!("{tabs}{}\n{tabs}{{\n", q(key)));
            for (k, vals) in obj.iter() {
                for v in vals { write_vdf(out, depth + 1, k, v); }
            }
            out.push_str(&format!("{tabs}}}\n"));
        }
    }
}

/// Walks (creating as needed) nested objects; Steam's keys are case-insensitive.
fn child<'a, 't>(obj: &'a mut Obj<'t>, key: &str) -> &'a mut Obj<'t> {
    let k = obj.keys().find(|k| k.eq_ignore_ascii_case(key)).cloned()
        .unwrap_or_else(|| Cow::Owned(key.to_owned()));
    let vals = obj.entry(k).or_default();
    if !vals.first().is_some_and(Value::is_obj) { *vals = vec![Value::Obj(Obj::new())]; }
    vals[0].get_mut_obj().unwrap()
}

fn set(obj: &mut Obj, key: &str, val: &str) {
    obj.retain(|k, _| !k.eq_ignore_ascii_case(key));
    obj.insert(Cow::Owned(key.to_owned()), vec![Value::Str(Cow::Owned(val.to_owned()))]);
}

/// Reads a string at `path` below the root of a VDF file; Steam keys are case-insensitive.
fn vdf_get(text: &str, path: &[&str]) -> Option<String> {
    let vdf = keyvalues_parser::parse(text).ok()?;
    let mut val = &vdf.value;
    for key in path {
        let obj = val.get_obj()?;
        val = obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(key))?.1.first()?;
    }
    val.get_str().map(str::to_owned)
}

/// Opens a URL (here steam:// ones, handled by Steam) with the desktop's default handler.
pub fn open_url(url: &str) {
    let mut c = Command::new("xdg-open");
    c.arg(url);
    spawn_detached(c);
}

/// skate.'s Wine prefix: in the Steam library that has skate., as Steam makes it on the first start.
fn skate_prefix() -> Option<PathBuf> {
    let dir = skate_dir().ok()??;
    Some(dir.parent()?.parent()?.join(format!("compatdata/{SKATE_APPID}/pfx")))
}

/// Points the prefix's Open dialog at our DLL (`dll`) and opens folders in the desktop's file manager, or puts back
/// Wine's own for both. Rewrites system.reg only when that changes it; Wine must not be running in the prefix, or it
/// overwrites the file when it exits.
fn set_prefix_dialog(pfx: &Path, dll: Option<&Path>) -> Result<()> {
    let path = pfx.join("system.reg");
    let text = fs::read_to_string(&path)?;
    // Wine sees the whole Linux filesystem as Z:.
    let value = dll.map_or(WINE_DIALOG.to_owned(), |d| format!("Z:{}", d.display()).replace('/', "\\"));
    let new = set_file_dialog(&text, &value).context("system.reg has no Open dialog class")?;
    let new = set_folder_open(&new, dll.is_some());
    if new != text { fs::write(&path, new)?; }
    Ok(())
}

/// `text` (a Wine .reg file) with the folder open command removed, then added back at the end with `on`.
fn set_folder_open(text: &str, on: bool) -> String {
    let (mut out, mut skip, mut ours) = (String::with_capacity(text.len()), false, false);
    for line in text.split_inclusive('\n') {
        if line.starts_with('[') { skip = line.get(..FOLDER_OPEN_KEY.len()).is_some_and(|k| k.eq_ignore_ascii_case(FOLDER_OPEN_KEY)); }
        if skip { ours |= line.trim_end() == FOLDER_OPEN; } else { out.push_str(line); }
    }
    // Already set: unchanged, so system.reg isn't rewritten on every start.
    if on && ours { return text.to_owned(); }
    if on {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        out.push_str(&format!("{FOLDER_OPEN_KEY} {now}\n{FOLDER_OPEN}\n\n"));
    }
    out
}

/// `text` (a Wine .reg file) with the Open dialog's InprocServer32 set to `dll`, or None without that key.
fn set_file_dialog(text: &str, dll: &str) -> Option<String> {
    let (mut out, mut in_key, mut done) = (String::with_capacity(text.len()), false, false);
    for line in text.split_inclusive('\n') {
        if line.starts_with('[') { in_key = line.get(..FILE_DIALOG_KEY.len()).is_some_and(|k| k.eq_ignore_ascii_case(FILE_DIALOG_KEY)); }
        if in_key && !done && line.starts_with("@=") {
            out.push_str(&format!("@=\"{}\"\n", dll.replace('\\', "\\\\")));
            done = true;
        } else { out.push_str(line); }
    }
    done.then_some(out)
}

/// Runs `cmd` (the rest of the launch option) and answers nativepicker.dll's requests until it exits. It runs
/// before Steam's container, so the desktop's picker is reachable here, unlike from inside the game.
fn picker_helper(cmd: &[String]) -> Result<()> {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
    let dir = std::env::current_exe()?.parent().context("no folder")?.to_owned();
    // A Proton update can rewrite the prefix's registry; Wine isn't running yet, so put the entry back.
    if let Some(data) = std::env::var_os("STEAM_COMPAT_DATA_PATH")
        && let Err(e) = set_prefix_dialog(&Path::new(&data).join("pfx"), Some(&dir.join("nativepicker.dll"))) {
        eprintln!("picker-helper: couldn't set skate.'s file dialog: {e}");
    }
    let mut child = Command::new(cmd.first().context("picker-helper: no command to run")?).args(&cmd[1..]).spawn()?;
    // The DLL falls back to Wine's dialog once this goes stale (3 s).
    let (alive, stop) = (dir.join("picker.alive"), Arc::new(AtomicBool::new(false)));
    let beat = { let (alive, stop) = (alive.clone(), stop.clone()); std::thread::spawn(move || while !stop.load(Ordering::Relaxed) {
        let _ = fs::File::options().create(true).write(true).open(&alive).and_then(|f| f.set_modified(std::time::SystemTime::now()));
        std::thread::sleep(std::time::Duration::from_secs(1));
    }) };
    let status = loop {
        if let Some(status) = child.try_wait()? { break status; }
        let request = dir.join("picker.request");
        if let Ok(text) = fs::read_to_string(&request) {
            let _ = fs::remove_file(&request);
            if let Some(answer) = picker_answer(&text) {
                fs::write(dir.join("picker.response.tmp"), answer)?;
                fs::rename(dir.join("picker.response.tmp"), dir.join("picker.response"))?;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    stop.store(true, Ordering::Relaxed);
    let _ = beat.join();
    let _ = fs::remove_file(&alive);
    std::process::exit(status.code().unwrap_or(1));
}

/// Shows the desktop's picker for a request (id, "file"/"folder", title, filter name, filter like `*.zip;*.r2z`) and
/// returns the response: id, then "ok" and the path, or "cancel".
fn picker_answer(request: &str) -> Option<String> {
    let mut lines = request.lines();
    let (id, kind, title) = (lines.next()?, lines.next()?, lines.next()?);
    let (filter_name, filter) = (lines.next().unwrap_or(""), lines.next().unwrap_or(""));
    let mut dialog = rfd::FileDialog::new().set_title(title);
    let picked = if kind == "folder" { dialog.pick_folder() } else {
        let exts: Vec<&str> = filter.split(';').map(|p| p.trim().trim_start_matches("*.")).filter(|e| !e.is_empty() && *e != "*").collect();
        if !exts.is_empty() { dialog = dialog.add_filter(filter_name, &exts); }
        dialog.pick_file()
    };
    Some(match picked {
        Some(p) => format!("{id}\nok\n{}\n", p.display()),
        None => format!("{id}\ncancel\n"),
    })
}

fn data_dir() -> Result<PathBuf> {
    Ok(dirs::data_dir().context("no data dir")?.join("reskate-installer"))
}

fn github_json(path: &str) -> Result<serde_json::Value> {
    Ok(serde_json::from_reader(ureq::get(&format!("https://api.github.com/repos/{path}"))
        .header("User-Agent", "reskate-installer").call()?.into_body().into_reader())?)
}

/// Downloads a release asset, checked against GitHub's sha256 digest.
fn github_asset(rel: &serde_json::Value, name: &str) -> Result<Vec<u8>> {
    let asset = rel["assets"].as_array().into_iter().flatten().find(|a| a["name"] == name)
        .with_context(|| format!("release {} has no {name}", rel["tag_name"]))?;
    let url = asset["browser_download_url"].as_str().context("no download url")?;
    let want = asset["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).context("release asset has no sha256 digest")?;
    let bytes = ureq::get(url).call()?.into_body().with_config().limit(1 << 30).read_to_vec()?;
    let got: String = sha2::Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
    if got != want { bail!("checksum mismatch for {name}"); }
    Ok(bytes)
}

#[test]
fn launch_option_swaps_anticheat() {
    // Steam runs launch options through a shell with %command% replaced by the quoted command line.
    for (pinned, tail) in [(false, ""), (true, " --no-update")] {
        let opt = launch_option(Path::new("/home/me/My Games/ReSkate"), pinned, false, false);
        let cmd = opt.replace("%command%", "echo proton waitforexitandrun '/lib/common/skate./EAAntiCheat.GameServiceLauncher.exe'");
        let out = Command::new("sh").args(["-c", &cmd]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("proton waitforexitandrun /home/me/My Games/ReSkate/ReSkateLauncher.exe{tail}\n"));
    }
}

#[test]
fn launch_option_runs_bridge() {
    // A stand-in bridge.sh: the real one also ends by running its arguments, with the bridge in its environment.
    let tmp = std::env::temp_dir().join(format!("reskate-installer-rpc-{}", std::process::id()));
    fs::create_dir_all(tmp.join(BRIDGE_DIR)).unwrap();
    fs::write(tmp.join(BRIDGE_SH), "BRIDGE_PATH=\"$(dirname \"$0\")/bridge.exe\" \"$@\"").unwrap();
    let opt = launch_option(&tmp, true, true, false);
    let cmd = opt.replace("%command%", "sh -c 'echo $BRIDGE_PATH \"$@\"' proton '/skate./EAAntiCheat.GameServiceLauncher.exe'");
    let out = Command::new("sh").args(["-c", &cmd]).output().unwrap();
    let (b, l) = (tmp.join(BRIDGE_DIR).join("bridge.exe"), tmp.join(LAUNCHER));
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{} {} --no-update\n", b.display(), l.display()), "{out:?}");
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn launch_option_runs_picker_helper() {
    // A stand-in helper that runs the rest like picker-helper does, after the bridge like with both on.
    let tmp = std::env::temp_dir().join(format!("reskate installer-picker-{}", std::process::id()));
    fs::create_dir_all(tmp.join(BRIDGE_DIR)).unwrap();
    fs::create_dir_all(tmp.join(PICKER_DIR)).unwrap();
    fs::write(tmp.join(BRIDGE_SH), "\"$@\"").unwrap();
    fs::write(tmp.join(PICKER_BIN), "#!/bin/sh\n[ \"$1\" = picker-helper ] && shift && echo helper && exec \"$@\"").unwrap();
    fs::set_permissions(tmp.join(PICKER_BIN), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let cmd = launch_option(&tmp, false, true, true).replace("%command%", "echo proton '/skate./EAAntiCheat.GameServiceLauncher.exe'");
    let out = Command::new("sh").args(["-c", &cmd]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), format!("helper\nproton {}\n", tmp.join(LAUNCHER).display()), "{out:?}");
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn file_dialog_reg_edit() {
    let reg = "[Software\\\\Classes\\\\CLSID\\\\{DC1C5A9C-E88A-4DDE-A5A1-60F82A20AEF7}] 1791936545\n@=\"File Open Dialog\"\n\n\
               [Software\\\\Classes\\\\CLSID\\\\{DC1C5A9C-E88A-4DDE-A5A1-60F82A20AEF7}\\\\InprocServer32] 1791936545\n#time=1dd5b7039ae0ae0\n\
               @=\"C:\\\\windows\\\\system32\\\\comdlg32.dll\"\n\"ThreadingModel\"=\"Apartment\"\n\n[Software\\\\Other] 1\n@=\"x\"\n";
    let ours = set_file_dialog(reg, r"Z:\home\me\Games\ReSkate\native-picker\nativepicker.dll").unwrap();
    assert_eq!(ours, reg.replace(r#"@="C:\\windows\\system32\\comdlg32.dll""#, r#"@="Z:\\home\\me\\Games\\ReSkate\\native-picker\\nativepicker.dll""#));
    assert_eq!(set_file_dialog(&ours, WINE_DIALOG).unwrap(), reg);
    assert_eq!(set_file_dialog("[Software\\\\Other] 1\n@=\"x\"\n", WINE_DIALOG), None);
}

#[test]
fn folder_open_reg_edit() {
    let reg = "[Software\\\\Classes\\\\file] 1\n@=\"FileMoniker\"\n\n[Software\\\\Classes\\\\folder\\\\shell\\\\open\\\\ddeexec] 1\n@=\"x\"\n\n";
    let on = set_folder_open(reg, true);
    assert!(on.starts_with(reg) && on.ends_with(&format!("{FOLDER_OPEN}\n\n")), "{on}");
    assert_eq!(set_folder_open(&on, true), on);
    assert_eq!(set_folder_open(&on, false), reg);
    // Wine writes keys lowercase or not, and with its own #time line.
    let wine = format!("{reg}[Software\\\\Classes\\\\Folder\\\\shell\\\\open\\\\command] 2\n#time=1d\n{FOLDER_OPEN}\n\n[Software\\\\Other] 1\n");
    assert_eq!(set_folder_open(&wine, true), wine);
    assert_eq!(set_folder_open(&wine, false), format!("{reg}[Software\\\\Other] 1\n"));
}

#[test]
fn versions() {
    assert!(supported("v1.0.1") && supported("v2.0.7") && supported("v10.0.0"));
    assert!(!supported("v1.0.0") && !supported("v0.9"));
}

#[test]
fn dir_checks() {
    let tmp = std::env::temp_dir().join(format!("reskate-installer-test-{}", std::process::id()));
    assert!(check_dir(&tmp).is_ok()); // doesn't exist yet
    assert!(check_dir(Path::new("Games/ReSkate")).is_err());
    assert!(check_dir(Path::new("/home/me/it's")).is_err());
    assert!(check_dir(Path::new("/mnt/lib/SteamApps/common/ReSkate")).is_err());
    fs::create_dir_all(&tmp).unwrap();
    fs::write(tmp.join("other.txt"), "").unwrap();
    assert!(check_dir(&tmp).is_err());
    fs::write(tmp.join("Skate.exe"), "").unwrap();
    assert!(check_dir(&tmp).is_ok());
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn extract_windows_zip() {
    use std::io::Write;
    let mut buf = std::io::Cursor::new(Vec::new());
    let mut w = zip::ZipWriter::new(&mut buf);
    for name in ["licenses\\a-LICENSE.txt", LAUNCHER, "..\\evil.txt"] {
        w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
        w.write_all(b"x").unwrap();
    }
    w.finish().unwrap();
    let tmp = std::env::temp_dir().join(format!("reskate-installer-zip-{}", std::process::id()));
    let files = extract(buf.get_ref(), &tmp.join("dir")).unwrap();
    assert_eq!(files, ["licenses/a-LICENSE.txt", LAUNCHER]);
    assert!(tmp.join("dir/licenses/a-LICENSE.txt").is_file() && !tmp.join("evil.txt").exists());
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn fixes_backslash_names() {
    let tmp = std::env::temp_dir().join(format!("reskate-installer-names-{}", std::process::id()));
    fs::create_dir_all(tmp.join("licenses")).unwrap();
    fs::create_dir_all(tmp.join("Mods/m/Win32\\levels")).unwrap();
    for f in ["licenses/a.txt", "licenses\\a.txt", "Mods/m/Win32\\globals.toc", "..\\evil.txt"] { fs::write(tmp.join(f), f).unwrap(); }
    assert_eq!(fix_names(&tmp, false).unwrap(), 3);
    assert!(tmp.join("licenses\\a.txt").exists() && broken_names(&tmp));
    assert_eq!(fix_names(&tmp, true).unwrap(), 3);
    assert!(!broken_names(&tmp));
    assert_eq!(fs::read_to_string(tmp.join("licenses/a.txt")).unwrap(), "licenses/a.txt");
    assert_eq!(fs::read_to_string(tmp.join("Mods/m/Win32/globals.toc")).unwrap(), "Mods/m/Win32\\globals.toc");
    assert!(!tmp.join("licenses\\a.txt").exists() && !tmp.join("Mods/m/Win32\\levels").exists() && tmp.join("..\\evil.txt").exists());
    fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn vdf_edits() {
    let cfg = vdf_edit("\"UserLocalConfigStore\"\n{\n}\n", |r| {
        set(child(child(r, "Software"), "valve"), "LaunchOptions", "bash -c 'exec \"x\"'");
    }).unwrap();
    assert!(cfg.contains("\"valve\"") && cfg.contains(r#""bash -c 'exec \"x\"'""#), "{cfg}");
    assert_eq!(vdf_get(&cfg, &["software", "Valve", "launchoptions"]).as_deref(), Some("bash -c 'exec \"x\"'"));
}

#[test]
#[ignore] // run with --ignored: round-trips this machine's real Steam account config
fn vdf_roundtrip_real() {
    let text = fs::read_to_string(local_config().unwrap()).unwrap();
    let out = vdf_edit(&text, |_| {}).unwrap();
    let norm = |s: &str| { let mut v: Vec<_> = s.lines().map(str::trim).filter(|l| !l.is_empty()).map(|l| l.split_whitespace().collect::<Vec<_>>().join(" ")).collect(); v.sort(); v };
    let (a, b) = (norm(&text), norm(&out));
    let diff: Vec<_> = a.iter().filter(|l| !b.contains(l)).chain(b.iter().filter(|l| !a.contains(l))).collect();
    assert!(diff.is_empty(), "{diff:?}");
    println!("skate. at {:?}, launch options {:?}", skate_dir().unwrap(), launch_options().unwrap());
}

#[test]
fn appinfo_reads_skate_build() {
    // An app entry in both formats: v28 has inline keys, v29 indexes a string table at the end.
    for v29 in [false, true] {
        let names = ["appinfo", "depots", "branches", "public", "buildid", GAME_DEPOT, "manifests", "gid", "size"];
        let key = |out: &mut Vec<u8>, k: &str| if v29 {
            out.extend((names.iter().position(|n| *n == k).unwrap() as u32).to_le_bytes());
        } else { out.extend(k.as_bytes()); out.push(0); };
        let mut kv = Vec::new();
        for (t, k) in [(0, "appinfo"), (0, "depots"), (0, "branches"), (0, "public")] { kv.push(t); key(&mut kv, k); }
        kv.push(2); key(&mut kv, "buildid"); kv.extend(25617022i32.to_le_bytes());
        kv.extend([8, 8]);
        for k in [GAME_DEPOT, "manifests", "public"] { kv.push(0); key(&mut kv, k); }
        kv.push(1); key(&mut kv, "gid"); kv.extend(b"4187479023593343011\0");
        kv.push(7); key(&mut kv, "size"); kv.extend(14863594925u64.to_le_bytes());
        kv.extend([8, 8, 8, 8, 8, 8]);
        let entry = |app: u32| [&app.to_le_bytes()[..], &(60 + kv.len() as u32).to_le_bytes(), &[0; 60], &kv].concat();
        let mut data = [&(if v29 { 0x0756_4429u32 } else { 0x0756_4428 }).to_le_bytes()[..], &1u32.to_le_bytes()].concat();
        let table_at = data.len();
        if v29 { data.extend(0u64.to_le_bytes()); }
        data.extend(entry(220));
        data.extend(entry(SKATE_APPID));
        data.extend(0u32.to_le_bytes());
        if v29 {
            let at = data.len() as u64;
            data[table_at..table_at + 8].copy_from_slice(&at.to_le_bytes());
            data.extend((names.len() as u32).to_le_bytes());
            for n in names { data.extend(n.as_bytes()); data.push(0); }
        }
        let (build, manifest) = appinfo_build(&data).unwrap();
        assert_eq!((build.as_str(), manifest.as_str()), ("25617022", "4187479023593343011"));
    }
    assert!(appinfo_build(&[0x29, 0x44, 0x56, 0x07, 1, 0, 0, 0]).is_err());
}

#[test]
fn placeholder_manifest_claims_build() {
    let fresh = placeholder_manifest("\"AppState\"\n{\n}\n", "2", "22", 7, 1).unwrap();
    // Steam's leftovers of an update are dropped, the anti-cheat depot is kept.
    let old = "\"AppState\"\n{\n\"buildid\" \"1\"\n\"StateFlags\" \"1190\"\n\"TargetBuildID\" \"2\"\n\"InstalledDepots\"\n{\n\
        \"3354751\"\n{\n\"manifest\" \"11\"\n}\n\"3893181\"\n{\n\"manifest\" \"9\"\n}\n}\n}\n";
    let updated = placeholder_manifest(old, "2", "22", 7, 1).unwrap();
    for acf in [&fresh, &updated] {
        assert_eq!(vdf_get(acf, &["buildid"]).as_deref(), Some("2"));
        assert_eq!(vdf_get(acf, &["StateFlags"]).as_deref(), Some("4"));
        assert_eq!(vdf_get(acf, &["InstalledDepots", GAME_DEPOT, "manifest"]).as_deref(), Some("22"));
        assert_eq!(vdf_get(acf, &["appid"]).as_deref(), Some("3354750"));
    }
    assert_eq!(vdf_get(&updated, &["InstalledDepots", "3893181", "manifest"]).as_deref(), Some("9"));
    assert!(vdf_get(&updated, &["TargetBuildID"]).is_none());
}

#[test]
#[ignore] // run with --ignored: reads this machine's real Steam app info cache
fn appinfo_real() {
    println!("skate. build and manifest: {:?}, placeholder {:?}, outdated {}", steam_build().unwrap(), placeholder(), placeholder_outdated());
}
