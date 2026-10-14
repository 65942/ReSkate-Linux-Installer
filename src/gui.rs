use eframe::egui::{self, Color32, CornerRadius, Margin, RichText, Stroke};
use std::io::{BufRead, BufReader, Read};
use std::process::{Command, Stdio};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const DEFAULT_ACCENT: u32 = 0x588cff;
/// Desktop accent color as 0xRRGGBB, updated by the theme watcher.
static ACCENT: AtomicU32 = AtomicU32::new(DEFAULT_ACCENT);

fn accent() -> Color32 {
    let c = ACCENT.load(Ordering::Relaxed);
    Color32::from_rgb((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// Black or white, whichever reads better on the accent.
fn on_accent() -> Color32 {
    let a = accent();
    let lum = 0.299 * a.r() as f32 + 0.587 * a.g() as f32 + 0.114 * a.b() as f32;
    if lum > 150.0 { Color32::from_rgb(15, 17, 22) } else { Color32::WHITE }
}

/// Colors for the current theme.
#[derive(Clone, PartialEq)]
struct Pal {
    dark: bool, bg: Color32, card: Color32, stroke: Color32, button: Color32, button_hover: Color32,
    muted: Color32, title: Color32, good: Color32, danger: Color32, log_bg: Color32, log_text: Color32,
}

/// The KDE color scheme, when running under KDE; replaces the built-in palette.
static KDE: Mutex<Option<Pal>> = Mutex::new(None);

fn builtin(dark: bool) -> Pal {
    let c = Color32::from_rgb;
    if dark {
        Pal { dark, bg: c(16, 18, 23), card: c(25, 28, 35), stroke: c(36, 40, 50), button: c(38, 42, 52), button_hover: c(50, 55, 68),
              muted: c(136, 143, 156), title: c(240, 242, 246), good: c(80, 200, 120), danger: c(240, 110, 110),
              log_bg: c(11, 12, 16), log_text: c(186, 192, 204) }
    } else {
        Pal { dark, bg: c(243, 245, 249), card: Color32::WHITE, stroke: c(224, 228, 236), button: c(234, 237, 243), button_hover: c(222, 226, 235),
              muted: c(98, 106, 120), title: c(20, 22, 28), good: c(28, 145, 78), danger: c(200, 50, 50),
              log_bg: c(234, 237, 243), log_text: c(50, 56, 68) }
    }
}

fn pal(ui: &egui::Ui) -> Pal {
    KDE.lock().unwrap().clone().unwrap_or_else(|| builtin(ui.visuals().dark_mode))
}

fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Reads `key=R,G,B` from `[Colors:<section>]` of kdeglobals.
fn kde_color(ini: &str, section: &str, key: &str) -> Option<Color32> {
    let header = format!("[Colors:{section}]");
    let body = ini.split(&header).nth(1)?.split("\n[").next()?;
    let val = body.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('='))?;
    let v: Vec<u8> = val.split(',').filter_map(|n| n.trim().parse().ok()).collect();
    (v.len() >= 3).then(|| Color32::from_rgb(v[0], v[1], v[2]))
}

/// Builds the palette (and accent) from KDE's active color scheme.
fn kde_palette(ini: &str) -> Option<(Pal, Color32)> {
    let get = |s, k| kde_color(ini, s, k);
    let (bg, text) = (get("Window", "BackgroundNormal")?, get("Window", "ForegroundNormal")?);
    let card = get("View", "BackgroundNormal").unwrap_or(bg);
    let button = get("Button", "BackgroundNormal").unwrap_or(card);
    let lum = |c: Color32| 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
    let pal = Pal {
        dark: lum(bg) < 128.0, bg, card, stroke: mix(card, text, 0.10), button, button_hover: mix(button, text, 0.12),
        // KDE's ForegroundInactive is often too faint for small text, so blend instead.
        muted: mix(text, card, 0.38), title: text,
        good: get("View", "ForegroundPositive").unwrap_or(Color32::from_rgb(80, 200, 120)),
        danger: get("View", "ForegroundNegative").unwrap_or(Color32::from_rgb(240, 110, 110)),
        log_bg: get("Window", "BackgroundAlternate").unwrap_or(mix(bg, Color32::BLACK, 0.2)), log_text: mix(text, card, 0.15),
    };
    let accent = get("Selection", "BackgroundNormal").or(get("View", "DecorationFocus")).unwrap_or(accent());
    Some((pal, accent))
}

/// Corner radii that match the desktop's widget style: (controls, cards).
fn radii() -> (u8, u8) {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    if desktop.contains("kde") { (3, 5) } // Breeze
    else if desktop.contains("gnome") { (6, 12) } // Adwaita
    else { (4, 8) }
}

/// UI font size in points, from the desktop (set by `load_system_fonts`).
static FONT_PT: AtomicU32 = AtomicU32::new(0);

fn base_px() -> f32 {
    match f32::from_bits(FONT_PT.load(Ordering::Relaxed)) {
        pt if pt > 0.0 => pt * 4.0 / 3.0, // points to pixels at 96 dpi; egui applies display scaling
        _ => 13.5,
    }
}

/// The desktop's UI (or monospace) font as (family, size in points).
fn desktop_font(mono: bool) -> Option<(String, f32)> {
    let parse_size = |s: &str| s.trim().parse::<f32>().ok();
    // KDE: kdeglobals `font=Noto Sans,10,...` / `fixed=Hack,10,...`; unset means Plasma's defaults.
        if std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase().contains("kde") {
            let ini = dirs::config_dir().and_then(|d| std::fs::read_to_string(d.join("kdeglobals")).ok()).unwrap_or_default();
            let key = if mono { "fixed=" } else { "font=" };
            let found = ini.lines().find_map(|l| l.strip_prefix(key)).and_then(|v| {
                let mut parts = v.split(',');
                Some((parts.next()?.to_owned(), parse_size(parts.next()?)?))
            });
        return Some(found.unwrap_or_else(|| (if mono { "Hack" } else { "Noto Sans" }.to_owned(), 10.0)));
    }
    // GNOME and others that use its settings: 'Cantarell 11'.
    let key = if mono { "monospace-font-name" } else { "font-name" };
    if let Ok(out) = Command::new("gsettings").args(["get", "org.gnome.desktop.interface", key]).output() {
        let v = String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_owned();
        if let Some((family, size)) = v.rsplit_once(' ').and_then(|(f, s)| Some((f.trim().to_owned(), parse_size(s)?))) {
            return Some((family, size));
        }
    }
    Some((if mono { "monospace" } else { "sans-serif" }.to_owned(), 10.0))
}

/// The font file for a family, from fontconfig.
fn font_file(family: &str) -> Option<Vec<u8>> {
    let out = Command::new("fc-match").args(["-f", "%{file}", family]).output().ok()?;
    std::fs::read(String::from_utf8(out.stdout).ok()?.trim()).ok()
}

/// Uses the desktop's fonts (egui's bundled ones stay as fallbacks for missing glyphs).
fn load_system_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (mono, family) in [(false, egui::FontFamily::Proportional), (true, egui::FontFamily::Monospace)] {
        let Some((name, pt)) = desktop_font(mono) else { continue };
        let Some(data) = font_file(&name) else { continue };
        let key = format!("system-{name}");
        fonts.font_data.insert(key.clone(), Arc::new(egui::FontData::from_owned(data)));
        fonts.families.entry(family).or_default().insert(0, key);
        if !mono { FONT_PT.store(pt.to_bits(), Ordering::Relaxed); }
    }
    ctx.set_fonts(fonts);
}

/// Follows the desktop's dark/light setting and accent color: KDE's color scheme, else the XDG settings portal
/// (GNOME and others). If neither is there, egui's own detection is used.
fn watch_desktop_theme(ctx: egui::Context) {
    let kdeglobals = dirs::config_dir().map(|d| d.join("kdeglobals"));
    thread::spawn(move || loop {
        let kde = kdeglobals.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).as_deref().and_then(kde_palette);
        let (theme, accent) = match &kde {
            Some((pal, acc)) => (
                if pal.dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light },
                (acc.r() as u32) << 16 | (acc.g() as u32) << 8 | acc.b() as u32,
            ),
            None => portal_theme(),
        };
        let kde = kde.map(|(pal, _)| pal);
        let changed = (ACCENT.swap(accent, Ordering::Relaxed) != accent)
            | (ctx.options(|o| o.theme_preference) != theme)
            | (*KDE.lock().unwrap() != kde);
        if changed {
            *KDE.lock().unwrap() = kde;
            ctx.set_theme(theme);
            style(&ctx);
            ctx.request_repaint();
        }
        thread::sleep(Duration::from_secs(3));
    });
}

/// Dark/light preference and accent from the XDG settings portal (GNOME and other non-KDE desktops).
fn portal_theme() -> (egui::ThemePreference, u32) {
    let read = |key: &str| -> Option<Vec<f64>> {
        // ponytail: shells out to busctl (systemd), add a D-Bus crate if non-systemd distros matter
        let out = Command::new("busctl").args(["--user", "call", "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop", "org.freedesktop.portal.Settings", "ReadOne", "ss", "org.freedesktop.appearance", key])
            .output().ok()?;
        // Output looks like `v u 1` or `v (ddd) 0.56 0.73 0.73`.
        let nums: Vec<f64> = String::from_utf8_lossy(&out.stdout).split_whitespace().filter_map(|t| t.parse().ok()).collect();
        (out.status.success() && !nums.is_empty()).then_some(nums)
    };
    let theme = match read("color-scheme").as_deref() {
        Some([1.0]) => egui::ThemePreference::Dark,
        Some([2.0]) => egui::ThemePreference::Light,
        _ => egui::ThemePreference::System,
    };
    // Components outside 0..1 mean "no accent set".
    let accent = match read("accent-color").as_deref() {
        Some(&[r, g, b]) if [r, g, b].iter().all(|c| (0.0..=1.0).contains(c)) =>
            ((r * 255.0) as u32) << 16 | ((g * 255.0) as u32) << 8 | (b * 255.0) as u32,
        _ => DEFAULT_ACCENT,
    };
    (theme, accent)
}

fn style(ctx: &egui::Context) {
    ctx.all_styles_mut(|s| {
        use egui::{FontId, TextStyle::*};
        let dark = s.visuals.dark_mode;
        let px = base_px();
        s.text_styles.insert(Heading, FontId::proportional(px * 1.6));
        s.text_styles.insert(Body, FontId::proportional(px));
        s.text_styles.insert(Button, FontId::proportional(px));
        s.text_styles.insert(Small, FontId::proportional(px * 0.88));
        s.text_styles.insert(Monospace, FontId::monospace(px * 0.92));
        s.spacing.button_padding = egui::vec2(px * 0.9, px * 0.42);
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.interact_size.y = px * 1.9;
        let (control, card) = radii();
        let p = KDE.lock().unwrap().clone().unwrap_or_else(|| builtin(dark));
        let v = &mut s.visuals;
        v.panel_fill = p.bg;
        v.window_fill = p.card;
        v.override_text_color = Some(p.title);
        v.extreme_bg_color = p.log_bg;
        v.selection.bg_fill = accent().gamma_multiply(0.85);
        v.selection.stroke = Stroke::new(1.0, on_accent());
        v.hyperlink_color = accent();
        v.window_stroke = Stroke::new(1.0, p.stroke);
        for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
            w.corner_radius = CornerRadius::same(control);
        }
        v.widgets.inactive.weak_bg_fill = p.button;
        v.widgets.inactive.bg_stroke = Stroke::NONE;
        v.widgets.hovered.weak_bg_fill = p.button_hover;
        v.widgets.hovered.bg_stroke = Stroke::NONE;
        v.widgets.active.weak_bg_fill = mix(p.button_hover, p.title, 0.1);
        v.widgets.open.weak_bg_fill = p.button_hover;
        v.window_corner_radius = CornerRadius::same(card);
        v.menu_corner_radius = CornerRadius::same(card);
    });
}
fn pipe(r: impl Read, log: &Mutex<String>, ctx: &egui::Context) {
    for line in BufReader::new(r).lines().map_while(Result::ok) {
        let mut l = log.lock().unwrap();
        l.push_str(&line);
        l.push('\n');
        ctx.request_repaint();
    }
}

fn card<R>(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let p = pal(ui);
    egui::Frame::new().fill(p.card).corner_radius(radii().1).inner_margin(Margin::symmetric(16, 12))
        .stroke(Stroke::new(1.0, p.stroke))
        .show(ui, |ui| { ui.set_width(ui.available_width()); body(ui) }).inner
}

fn section(ui: &mut egui::Ui, title: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(title.to_uppercase()).small().strong().color(pal(ui).muted));
}

fn primary(text: &str) -> egui::Button<'_> {
    egui::Button::new(RichText::new(text).color(on_accent()).strong()).fill(accent())
}

/// Width of the Install / Uninstall / Play buttons.
const BUTTON_WIDTH: f32 = 92.0;

pub fn run() -> eframe::Result {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([600.0, 540.0]).with_min_inner_size([480.0, 380.0]),
        ..Default::default()
    };
    let app = App::new();
    let releases = app.releases.clone();
    eframe::run_native("ReSkate Installer", opts, Box::new(|cc| {
        load_system_fonts(&cc.egui_ctx);
        style(&cc.egui_ctx);
        watch_desktop_theme(cc.egui_ctx.clone());
        // Fetched without blocking the UI; the version dropdown shows "Loading…" until then.
        let ctx = cc.egui_ctx.clone();
        thread::spawn(move || match crate::releases() {
            Ok(tags) => { *releases.lock().unwrap() = tags; ctx.request_repaint(); }
            Err(e) => eprintln!("couldn't list ReSkate releases: {e}"),
        });
        Ok(Box::new(app))
    }))
}

struct App {
    log: Arc<Mutex<String>>,
    busy: Arc<AtomicBool>,
    /// A task ran; install state is re-read once `busy` drops.
    ran: bool,
    /// skate.'s Steam folder (None = not installed), or why Steam wasn't found.
    skate: Result<Option<PathBuf>, String>,
    /// skate. is only a placeholder in Steam; `outdated`: skate. updated on Steam since.
    placeholder: bool,
    outdated: bool,
    /// ReSkate's folder, when installed.
    installed: Option<PathBuf>,
    /// skate.'s launch option still starts ReSkate.
    launch_ok: bool,
    /// The folder field, shown until installed.
    dir_input: String,
    /// Folder picked in the Browse dialog, which runs on its own thread.
    picked_folder: Arc<Mutex<Option<PathBuf>>>,
    show_log: bool,
    /// The uninstall dialog is open; with its "also delete the folder" box.
    confirm: bool,
    delete_folder: bool,
    /// A task waiting for the user to OK closing Steam: its CLI args.
    close_steam: Option<Vec<String>>,
    /// The version ReSkate is pinned to; None = the latest, updating itself.
    pinned: Option<String>,
    /// Installable releases, newest first, fetched in the background.
    releases: Arc<Mutex<Vec<String>>>,
    /// Version picked in the dropdown ("" = the latest).
    selected: String,
    /// Installed with Discord Rich Presence (rpc-bridge), and the checkbox for it.
    bridge: bool,
    rpc: bool,
    /// Install/Repair fixes file names with `\` in them (`--fix-names`); `broken_names`: ReSkate's folder has some.
    fix_names: bool,
    broken_names: bool,
}

impl App {
    fn new() -> Self {
        let mut app = App {
            log: Default::default(), busy: Default::default(), ran: false, skate: Err(String::new()), placeholder: false, outdated: false, installed: None,
            launch_ok: false, dir_input: crate::default_dir().display().to_string(), picked_folder: Default::default(),
            show_log: false, confirm: false, delete_folder: false, close_steam: None,
            pinned: None, releases: Default::default(), selected: String::new(), bridge: false, rpc: false, fix_names: true, broken_names: false,
        };
        app.refresh_status();
        app
    }

    fn refresh_status(&mut self) {
        self.skate = crate::skate_dir().map_err(|e| e.to_string());
        self.placeholder = crate::placeholder().is_some();
        self.outdated = crate::placeholder_outdated();
        self.installed = crate::installed();
        self.launch_ok = crate::launch_ok();
        self.pinned = crate::pinned();
        self.selected = self.pinned.clone().unwrap_or_default();
        self.bridge = crate::rpc();
        self.rpc = self.bridge;
        self.broken_names = self.installed.as_deref().is_some_and(crate::broken_names);
        if let Some(d) = &self.installed { self.dir_input = d.display().to_string(); }
    }

    /// Runs this same binary with CLI args in the background and streams its output into the log.
    fn start(&mut self, ctx: &egui::Context, args: &[&str]) {
        self.ran = true;
        self.busy.store(true, Ordering::SeqCst);
        let (log, busy, ctx) = (self.log.clone(), self.busy.clone(), ctx.clone());
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        log.lock().unwrap().push_str(&format!("> {}\n", args.join(" ")));
        thread::spawn(move || {
            let push = |s: &str| { log.lock().unwrap().push_str(s); ctx.request_repaint(); };
            let result = (|| -> std::io::Result<bool> {
                let mut child = Command::new(std::env::current_exe()?)
                    .args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
                let err = child.stderr.take().unwrap();
                let (log2, ctx2) = (log.clone(), ctx.clone());
                let errs = thread::spawn(move || pipe(err, &log2, &ctx2));
                pipe(child.stdout.take().unwrap(), &log, &ctx);
                let _ = errs.join();
                Ok(child.wait()?.success())
            })();
            match result {
                Ok(true) => push("done\n\n"),
                Ok(false) => push("failed\n\n"),
                Err(e) => push(&format!("failed: {e}\n\n")),
            }
            busy.store(false, Ordering::SeqCst);
            ctx.request_repaint();
        });
    }

    /// Starts a task that edits Steam's config; if Steam is running, first asks to close it.
    fn steam_task(&mut self, ctx: &egui::Context, args: &[&str]) {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        if crate::steam_running() { self.close_steam = Some(args); } else { self.start(ctx, &args.iter().map(String::as_str).collect::<Vec<_>>()); }
    }

    /// Installs `version` ("" = the latest); into the folder field's folder until installed.
    fn install(&mut self, ctx: &egui::Context, version: &str) {
        let mut args = vec!["install".to_owned()];
        if self.installed.is_none() { args.push(self.dir_input.trim().to_owned()); }
        if !version.is_empty() { args.extend(["--version".to_owned(), version.to_owned()]); }
        if self.rpc { args.push("--rpc".to_owned()); }
        if self.fix_names { args.push("--fix-names".to_owned()); }
        self.steam_task(ctx, &args.iter().map(String::as_str).collect::<Vec<_>>());
    }

    /// Last meaningful log line, for the status bar.
    fn status_line(&self) -> String {
        let log = self.log.lock().unwrap();
        log.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("Ready").chars().take(90).collect()
    }

    /// ReSkate: state and Install / Repair / Uninstall / Play, then the folder field until installed.
    fn reskate_card(&mut self, ui: &mut egui::Ui, busy: bool) {
        let p = pal(ui);
        let dir = PathBuf::from(self.dir_input.trim());
        // Shown as a sentence, so capitalized (CLI errors read "Error: use a full path").
        let dir_ok = crate::check_dir(&dir).map_err(|e| { let e = e.to_string(); e[..1].to_uppercase() + &e[1..] });
        // Only flagged while the fix is on, so unchecking it stops the nagging.
        let broken = self.fix_names && self.broken_names;
        ui.horizontal(|ui| {
            ui.set_min_height(46.0);
            ui.vertical(|ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("ReSkate").strong().color(p.title));
                    if self.installed.is_some() {
                        // Painted dot: the default font has no bullet glyph.
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                        let ok = self.launch_ok && !self.outdated && !broken;
                        ui.painter().circle_filled(rect.center(), 4.0, if ok { p.good } else { p.danger });
                        resp.on_hover_text(if ok { "Installed" } else if self.outdated { "Placeholder outdated" }
                            else if broken { "Broken file names" } else { "Launch option missing" });
                    }
                });
                let sub = match &self.installed {
                    Some(_) if !self.launch_ok => "skate.'s launch option no longer starts ReSkate: press Repair".into(),
                    Some(_) if self.outdated => "skate. was updated on Steam: press Repair, or Steam downloads all 15 GB of it".into(),
                    Some(_) if broken => "Some files in ReSkate's folder have broken names, which break mods: press Repair".into(),
                    Some(d) => format!("Installed in {} · {}", d.display(),
                        self.pinned.as_ref().map_or("updates itself".to_owned(), |v| format!("pinned to {v}"))),
                    None => "Offline play, multiplayer and mods for skate.".into(),
                };
                let bad = self.installed.is_some() && (!self.launch_ok || self.outdated || broken);
                ui.label(RichText::new(sub).small().color(if bad { p.danger } else { p.muted }));
            });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if busy {
                    ui.spinner();
                    ui.label(RichText::new("Working…").small().color(p.muted));
                    return;
                }
                let size = egui::vec2(BUTTON_WIDTH, 0.0);
                match &self.installed {
                    None => {
                        let can = match (&self.skate, &dir_ok) {
                            (Err(e), _) => Err(e.clone()),
                            (_, Err(e)) => Err(e.clone()),
                            _ => Ok(()),
                        };
                        let b = ui.add_enabled(can.is_ok(), primary("Install").min_size(size));
                        let b = if let Err(why) = &can { b.on_disabled_hover_text(why) } else { b };
                        if b.clicked() { self.install(ui.ctx(), &self.selected.clone()); }
                    }
                    Some(_) => {
                        let b = egui::Button::new(RichText::new("Uninstall").color(p.danger)).fill(p.danger.gamma_multiply(0.12)).min_size(size);
                        if ui.add(b).clicked() { self.delete_folder = false; self.confirm = true; }
                        let current = self.pinned.clone().unwrap_or_default();
                        if self.selected != current || self.rpc != self.bridge {
                            let label = if self.selected != current { "Switch" } else { "Apply" };
                            if ui.add(primary(label).min_size(size)).clicked() { self.install(ui.ctx(), &self.selected.clone()); }
                        } else if !self.launch_ok || self.outdated || broken {
                            if ui.add(primary("Repair").min_size(size)).on_hover_text("Download ReSkate again and set the launch option").clicked() {
                                self.install(ui.ctx(), &current);
                            }
                        } else if ui.add(primary("Play").min_size(size)).on_hover_text("Start skate. through Steam").clicked() {
                            crate::open_url(&format!("steam://rungameid/{}", crate::SKATE_APPID));
                        }
                    }
                }
                let tags = self.releases.lock().unwrap().clone();
                let latest = tags.first().map_or_else(|| "Latest".to_owned(), |t| format!("{t} (latest)"));
                let shown = if self.selected.is_empty() { latest.clone() } else { self.selected.clone() };
                ui.add_enabled_ui(!tags.is_empty(), |ui| {
                    egui::ComboBox::from_id_salt("version").width(130.0).selected_text(shown).show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.selected, String::new(), latest).on_hover_text("Updates itself");
                        for t in tags.iter().skip(1) { ui.selectable_value(&mut self.selected, t.clone(), t); }
                    }).response.on_hover_text("An older version is pinned: ReSkate won't update itself while it's picked.");
                });
            });
        });
        ui.separator();
        // Proton splits rpc-bridge's command on spaces.
        let spaces = self.dir_input.contains(char::is_whitespace);
        if spaces { self.rpc = false; }
        ui.add_enabled(!spaces, egui::Checkbox::new(&mut self.rpc, "Discord Rich Presence"))
            .on_hover_text("Installs rpc-bridge, so ReSkate can show what you're playing in Discord or Vesktop")
            .on_disabled_hover_text("Needs a folder without spaces in its path");
        ui.checkbox(&mut self.fix_names, "Fix broken file names")
            .on_hover_text("On Install and Repair, fixes files whose names contain \\ (left by some Linux unzip tools). \
                They stop ReSkate from combining mods, so only one custom map or cosmetic mod works.");
        if self.installed.is_some() { return; }
        ui.separator();
        ui.label(RichText::new("Folder").small().color(p.muted));
        ui.horizontal(|ui| {
            if ui.button("Browse…").clicked() {
                // The dialog blocks, so keep it off the UI thread.
                let (slot, ctx) = (self.picked_folder.clone(), ui.ctx().clone());
                thread::spawn(move || {
                    if let Some(d) = rfd::FileDialog::new().set_title("Choose a new, empty folder for ReSkate").pick_folder() {
                        *slot.lock().unwrap() = Some(d);
                        ctx.request_repaint();
                    }
                });
            }
            ui.add(egui::TextEdit::singleline(&mut self.dir_input).hint_text("A new, empty folder").desired_width(f32::INFINITY));
        });
        let msg = match &dir_ok {
            Ok(()) => "ReSkate installs its own copy of the game here (about 14 GB), so Steam updating skate. can't break it.".to_owned(),
            Err(e) => e.clone(),
        };
        ui.label(RichText::new(msg).small().color(if dir_ok.is_ok() { p.muted } else { p.danger }));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let busy = self.busy.load(Ordering::SeqCst);
        if !busy && std::mem::take(&mut self.ran) { self.refresh_status(); }
        if let Some(d) = self.picked_folder.lock().unwrap().take() { self.dir_input = d.display().to_string(); }
        let p = pal(ui);

        // Status bar with the latest log line; click to expand the full log.
        egui::Panel::bottom("status").frame(egui::Frame::new().fill(p.card).inner_margin(Margin::symmetric(20, 10))
            .stroke(Stroke::new(1.0, p.stroke))).show(ui, |ui| {
            ui.horizontal(|ui| {
                if busy { ui.spinner(); }
                ui.label(RichText::new(self.status_line()).small().color(p.muted));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let toggle = egui::Button::new(RichText::new(if self.show_log { "Hide log" } else { "Show log" }).small()).frame(false);
                    if ui.add(toggle).clicked() { self.show_log = !self.show_log; }
                });
            });
            if self.show_log {
                egui::Frame::new().fill(p.log_bg).corner_radius(radii().1).inner_margin(Margin::same(10)).show(ui, |ui| {
                    egui::ScrollArea::vertical().max_height(220.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                        ui.add(egui::Label::new(RichText::new(self.log.lock().unwrap().as_str()).monospace().color(p.log_text)).wrap());
                    });
                });
            }
        });

        if self.confirm {
            let modal = egui::Modal::new(egui::Id::new("confirm")).show(ui.ctx(), |ui| {
                ui.label(RichText::new("Uninstall ReSkate?").strong().color(p.title));
                ui.add_space(8.0);
                ui.label(RichText::new("skate.'s launch option is put back the way it was.").small().color(p.muted));
                ui.checkbox(&mut self.delete_folder, "Also delete the ReSkate folder: its copy of the game (about 14 GB), mods and settings");
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    let b = egui::Button::new(RichText::new("Uninstall").color(p.danger)).fill(p.danger.gamma_multiply(0.12));
                    if ui.add_enabled(!busy, b).clicked() {
                        self.steam_task(ui.ctx(), if self.delete_folder { &["off", "--delete"] } else { &["off"] });
                        self.confirm = false;
                    }
                    if ui.button("Cancel").clicked() { self.confirm = false; }
                });
            });
            if modal.should_close() { self.confirm = false; }
        }

        if let Some(args) = self.close_steam.clone() {
            let modal = egui::Modal::new(egui::Id::new("close_steam")).show(ui.ctx(), |ui| {
                ui.label(RichText::new("Steam needs to close").strong().color(p.title));
                ui.add_space(8.0);
                ui.label("skate.'s launch option can only be changed while Steam is closed, since Steam overwrites its settings when it exits. \
                    The installer closes Steam the way its Exit menu does, then starts it again.");
                ui.label(RichText::new("Downloads pause, and a game that's running may be closed.").small().color(p.muted));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!busy, primary("Close Steam and continue")).clicked() {
                        let args: Vec<&str> = args.iter().map(String::as_str).chain(["--close-steam"]).collect();
                        self.start(ui.ctx(), &args);
                        self.close_steam = None;
                    }
                    if ui.button("Cancel").clicked() { self.close_steam = None; }
                });
            });
            if modal.should_close() { self.close_steam = None; }
        }

        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg).inner_margin(Margin::symmetric(24, 20))).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 10.0;
            ui.heading(RichText::new("ReSkate Installer").strong().color(p.title));
            section(ui, "skate.");
            card(ui, |ui| match &self.skate {
                Ok(Some(d)) if self.placeholder => {
                    ui.label(RichText::new(format!("Placeholder in {}", d.display())).small().color(p.good));
                    ui.label(RichText::new("ReSkate plays its own copy, so Steam only keeps EA's anti-cheat. \
                        Don't use Verify integrity of game files on skate.: it downloads all 15 GB.").small().color(p.muted));
                }
                Ok(Some(d)) => { ui.label(RichText::new(format!("Found in {}", d.display())).small().color(p.good)); }
                Ok(None) => {
                    ui.label(RichText::new("Not installed in Steam. Install adds a placeholder instead: Steam needs one to start ReSkate, \
                        and downloads only EA's anti-cheat for it (~350 MB) rather than the whole game (15 GB).").small().color(p.muted));
                    if ui.button("Check again").clicked() { self.refresh_status(); }
                }
                Err(e) => {
                    ui.label(RichText::new(e).small().color(p.danger));
                    if ui.button("Check again").clicked() { self.refresh_status(); }
                }
            });
            section(ui, "ReSkate");
            card(ui, |ui| self.reskate_card(ui, busy));
            ui.label(RichText::new("Steam is closed while the launch option is changed (you're asked first) and started again after. \
                Then press Play on skate. in Steam. ReSkate's launcher opens in place of EA's anti-cheat, which doesn't run under Proton, \
                and installs its own copy of the game the first time. It keeps itself up to date.").small().color(p.muted));
        });
    }
}

#[test]
fn start_streams_child_output() {
    // In tests current_exe is the test harness, so `--list` makes the child print test names.
    let mut app = App::new();
    app.start(&egui::Context::default(), &["--list"]);
    while app.busy.load(Ordering::SeqCst) { thread::sleep(Duration::from_millis(20)); }
    let log = app.log.lock().unwrap();
    assert!(log.contains("vdf_edits: test") && log.ends_with("done\n\n"), "{log}");
}

#[test]
fn kde_scheme() {
    let ini = "[Colors:Selection]\nBackgroundNormal=143,188,187\n\n[Colors:Window]\nBackgroundNormal=59,66,82\nForegroundNormal=216,222,233\n[General]\nX=1\n";
    let (p, acc) = kde_palette(ini).unwrap();
    assert!(p.dark && p.bg == Color32::from_rgb(59, 66, 82) && acc == Color32::from_rgb(143, 188, 187));
    assert!(kde_palette("[General]\n").is_none());
}
