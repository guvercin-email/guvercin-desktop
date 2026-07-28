//! Linux and the BSDs: freedesktop.org desktop entries, `xdg-mime`/`xdg-settings`
//! associations, the file-manager entries for GNOME/KDE/Cinnamon/XFCE, the
//! launcher badge, and removing the installed app.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI64, Ordering};

use super::shared;
use super::{AppRemoval, DefaultMailOutcome};

/// Our own desktop entry — the one that carries the mail associations and the
/// icon, and the name we hand to `xdg-mime`.
const DESKTOP_FILE: &str = "guvercin.desktop";
const NAUTILUS_SCRIPT: &str = shared::CONTEXT_MENU_LABEL;
const KDE_SERVICE_FILE: &str = "guvercin-attach.desktop";
const NEMO_ACTION_FILE: &str = "guvercin-attach.nemo_action";
/// Icon name the desktop entry, the launcher and notifications all look up.
/// `linux_desktop_entry` writes `Icon=<product name>`, and the deb/rpm bundlers
/// install the packaged icon under that same name, so the two must agree.
const ICON_NAME: &str = "guvercin";

/// The application icon, carried in the binary so the desktop integration has
/// one to install even when nothing unpacked an icon next to us — which is
/// exactly the AppImage and "copied the binary somewhere" cases, where a
/// package manager never ran. Without it the launcher entry, the Default
/// Applications list and every notification show a blank placeholder.
const ICON_PNG_128: &[u8] = include_bytes!("../../icons/128x128.png");
const ICON_PNG_256: &[u8] = include_bytes!("../../icons/128x128@2x.png");

fn home() -> Result<PathBuf, String> {
  std::env::var_os("HOME")
    .map(PathBuf::from)
    .ok_or_else(|| "HOME is not set".to_string())
}

/// `$XDG_DATA_HOME`, or `~/.local/share` when it is unset — where per-user
/// desktop entries, file-manager scripts and service menus belong.
fn data_home() -> Result<PathBuf, String> {
  if let Some(dir) = std::env::var_os("XDG_DATA_HOME") {
    let path = PathBuf::from(dir);
    if path.is_absolute() {
      return Ok(path);
    }
  }
  Ok(home()?.join(".local/share"))
}

fn config_home() -> Result<PathBuf, String> {
  if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME") {
    let path = PathBuf::from(dir);
    if path.is_absolute() {
      return Ok(path);
    }
  }
  Ok(home()?.join(".config"))
}

fn applications_dir() -> Result<PathBuf, String> {
  Ok(data_home()?.join("applications"))
}

/// The command every integration should launch. Inside an AppImage the running
/// executable lives in a temporary mount that disappears on exit, so `$APPIMAGE`
/// — the path of the AppImage file itself — is the only durable entry point.
fn exec_path() -> Result<String, String> {
  if let Some(appimage) = std::env::var_os("APPIMAGE") {
    let path = PathBuf::from(appimage);
    if path.exists() {
      return Ok(path.to_string_lossy().to_string());
    }
  }
  std::env::current_exe()
    .map(|exe| exe.to_string_lossy().to_string())
    .map_err(|e| e.to_string())
}

/// The `WM_CLASS` our window reports. GTK takes it from the program name, which
/// is the running executable's file stem — inside an AppImage that is the
/// binary within, not the `.AppImage` file the desktop entry launches.
fn wm_class() -> String {
  std::env::current_exe()
    .ok()
    .and_then(|exe| exe.file_stem().map(|s| s.to_string_lossy().to_string()))
    .filter(|stem| !stem.is_empty())
    .unwrap_or_else(|| ICON_NAME.to_string())
}

/// Desktop-entry names that mean "guvercin": ours, and the one the deep-link
/// plugin writes when it registers the URI schemes at startup.
fn our_desktop_files() -> Vec<String> {
  let mut names = vec![DESKTOP_FILE.to_string()];
  if let Ok(exe) = std::env::current_exe() {
    if let Some(file_name) = exe.file_name().and_then(|n| n.to_str()) {
      names.push(format!("{file_name}-handler.desktop"));
    }
  }
  names
}

/// Whether `bin` is on `PATH`, without spawning anything.
fn has_command(bin: &str) -> bool {
  let Some(path) = std::env::var_os("PATH") else {
    return false;
  };
  std::env::split_paths(&path).any(|dir| dir.join(bin).is_file())
}

fn run(bin: &str, args: &[&str]) -> Result<String, String> {
  if !has_command(bin) {
    return Err(format!("{bin} is not installed"));
  }
  let output = Command::new(bin)
    .args(args)
    .output()
    .map_err(|e| format!("{bin} could not be run: {e}"))?;
  if !output.status.success() {
    return Err(format!(
      "{bin} failed: {}",
      String::from_utf8_lossy(&output.stderr).trim()
    ));
  }
  Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn write_executable(path: &Path, contents: &str) -> Result<(), String> {
  use std::os::unix::fs::PermissionsExt;
  fs::write(path, contents).map_err(|e| e.to_string())?;
  fs::set_permissions(path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())
}

/// `~/.local/share/icons/hicolor/<size>/apps/guvercin.png` for each size we
/// carry.
fn icon_paths() -> Result<Vec<(PathBuf, &'static [u8])>, String> {
  let hicolor = data_home()?.join("icons/hicolor");
  Ok(vec![
    (
      hicolor.join("128x128/apps").join(format!("{ICON_NAME}.png")),
      ICON_PNG_128,
    ),
    (
      hicolor.join("256x256/apps").join(format!("{ICON_NAME}.png")),
      ICON_PNG_256,
    ),
  ])
}

/// Installs the application icon into the user's icon theme, unless a package
/// manager already put one in a system directory. Best effort: a desktop entry
/// without an icon still works, it just looks unfinished.
fn install_icon() {
  // A packaged install (`/usr/share/icons/...`) already has one, and the
  // system copy wins nothing by being shadowed.
  let system_icon = ["/usr/share", "/usr/local/share"].iter().any(|prefix| {
    Path::new(prefix)
      .join("icons/hicolor/128x128/apps")
      .join(format!("{ICON_NAME}.png"))
      .exists()
  });
  if system_icon {
    return;
  }

  let Ok(icons) = icon_paths() else { return };
  let mut wrote_any = false;
  for (path, bytes) in icons {
    let Some(dir) = path.parent() else { continue };
    if fs::create_dir_all(dir).is_err() {
      continue;
    }
    match fs::write(&path, bytes) {
      Ok(()) => wrote_any = true,
      Err(e) => log::debug!("icon: could not write {}: {e}", path.display()),
    }
  }

  if wrote_any {
    if let Ok(hicolor) = data_home().map(|dir| dir.join("icons/hicolor")) {
      // GTK notices a new icon on its own once the theme's mtime changes, so a
      // missing gtk-update-icon-cache is not a problem.
      let _ = run("gtk-update-icon-cache", &["-q", "-t", "-f", &hicolor.to_string_lossy()]);
    }
  }
}

fn remove_icon() {
  let Ok(icons) = icon_paths() else { return };
  for (path, _) in icons {
    if path.exists() {
      let _ = fs::remove_file(&path);
    }
  }
}

/// Writes (or refreshes) our desktop entry and tells the desktop about it.
fn install_desktop_entry(app: &tauri::AppHandle) -> Result<PathBuf, String> {
  let dir = applications_dir()?;
  fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

  let product = app
    .config()
    .product_name
    .clone()
    .unwrap_or_else(|| "guvercin".to_string());
  let path = dir.join(DESKTOP_FILE);
  fs::write(
    &path,
    shared::linux_desktop_entry(&exec_path()?, &product, &wm_class()),
  )
  .map_err(|e| e.to_string())?;

  // The entry names an icon; make sure the icon it names exists.
  install_icon();

  if let Err(e) = run("update-desktop-database", &[&dir.to_string_lossy()]) {
    // Only a cache refresh; the entry is still valid without it.
    log::debug!("desktop entry: {e}");
  }
  Ok(path)
}

pub fn is_default_mail_client(_app: &tauri::AppHandle) -> bool {
  let ours = our_desktop_files();

  if let Ok(out) = run("xdg-settings", &["get", "default-url-scheme-handler", "mailto"]) {
    if shared::desktop_handler_matches(&out, &ours) {
      return true;
    }
  }
  if let Ok(out) = run("xdg-mime", &["query", "default", "x-scheme-handler/mailto"]) {
    if shared::desktop_handler_matches(&out, &ours) {
      return true;
    }
  }

  // Neither tool present (a minimal desktop): read the file they would write.
  let Ok(list) = config_home().map(|dir| dir.join("mimeapps.list")) else {
    return false;
  };
  let Ok(text) = fs::read_to_string(list) else {
    return false;
  };
  shared::mimeapps_default_for(&text, "x-scheme-handler/mailto")
    .map(|entry| ours.iter().any(|name| name == &entry))
    .unwrap_or(false)
}

/// Claims `mailto:`, the `guvercin:` scheme and `.eml` files.
///
/// `xdg-settings`/`xdg-mime` are the supported route and are what desktops
/// watch for changes; writing `mimeapps.list` ourselves is the fallback for
/// systems where neither is installed, since that file is what both tools
/// ultimately edit.
pub fn set_as_default_mail_client(app: &tauri::AppHandle) -> Result<DefaultMailOutcome, String> {
  install_desktop_entry(app)?;

  const MIME_TYPES: [&str; 3] = [
    "x-scheme-handler/mailto",
    "x-scheme-handler/guvercin",
    "message/rfc822",
  ];

  let mut tool_errors: Vec<String> = vec![];

  if let Err(e) = run(
    "xdg-settings",
    &["set", "default-url-scheme-handler", "mailto", DESKTOP_FILE],
  ) {
    tool_errors.push(e);
  }
  for mime in MIME_TYPES {
    if let Err(e) = run("xdg-mime", &["default", DESKTOP_FILE, mime]) {
      tool_errors.push(e);
    }
  }

  if !tool_errors.is_empty() {
    // Fall back to editing mimeapps.list directly.
    let dir = config_home()?;
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = dir.join("mimeapps.list");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let updated = shared::upsert_mimeapps_defaults(&existing, DESKTOP_FILE, &MIME_TYPES);
    fs::write(&path, updated).map_err(|e| e.to_string())?;
    log::info!("default mail: wrote mimeapps.list directly ({})", tool_errors.join("; "));
  }

  if is_default_mail_client(app) {
    return Ok(DefaultMailOutcome::done());
  }
  Ok(DefaultMailOutcome::pending(
    "guvercin is registered as a mail app, but your desktop has not picked the change up yet. \
     It usually applies after you log out and back in, or you can select guvercin under \
     Default Applications in your system settings.",
  ))
}

/// The per-file-manager drop-in files, in the order they are written.
fn context_menu_files() -> Result<Vec<PathBuf>, String> {
  let data = data_home()?;
  Ok(vec![
    data.join("nautilus/scripts").join(NAUTILUS_SCRIPT),
    data.join("kio/servicemenus").join(KDE_SERVICE_FILE),
    data.join("nemo/actions").join(NEMO_ACTION_FILE),
  ])
}

/// Thunar keeps every custom action in this one file, shared with the user's
/// own, so it is edited rather than written.
fn thunar_uca_path() -> Result<PathBuf, String> {
  Ok(config_home()?.join("Thunar/uca.xml"))
}

/// Asks KDE to re-read its service menus, the same courtesy `pbs -flush` does
/// for the macOS Services menu — without it Dolphin shows the new entry only
/// after the next login. Absent on non-KDE systems, so failure is ignored.
///
/// Rebuilding the cache takes on the order of a second, and this is called from
/// first-launch setup, so it runs on its own thread rather than holding up the
/// window. The thread waits on the child so it is reaped rather than left as a
/// zombie for the lifetime of the app.
fn flush_kde_services() {
  for bin in ["kbuildsycoca6", "kbuildsycoca5"] {
    if has_command(bin) {
      std::thread::spawn(move || {
        if let Err(e) = Command::new(bin).arg("--noincremental").output() {
          log::debug!("context menu: {bin} could not refresh the KDE cache: {e}");
        }
      });
      return;
    }
  }
}

/// Installs "Send with guvercin" for every file manager that reads a per-user
/// drop-in: GNOME Files (Nautilus), Dolphin (KDE), Nemo (Cinnamon) and Thunar
/// (XFCE). Each one is best effort — a desktop without that file manager simply
/// never reads the file — but a failure to write any of them is reported, since
/// the user asked for the entry.
pub fn register_context_menu(app: &tauri::AppHandle) -> Result<(), String> {
  let exec = exec_path()?;
  let data = data_home()?;

  // The entry launches guvercin through its desktop entry / URI scheme, so make
  // sure that exists first.
  install_desktop_entry(app)?;

  let nautilus_dir = data.join("nautilus/scripts");
  fs::create_dir_all(&nautilus_dir).map_err(|e| e.to_string())?;
  write_executable(
    &nautilus_dir.join(NAUTILUS_SCRIPT),
    &shared::linux_nautilus_script(),
  )?;

  let kde_dir = data.join("kio/servicemenus");
  fs::create_dir_all(&kde_dir).map_err(|e| e.to_string())?;
  let kde_path = kde_dir.join(KDE_SERVICE_FILE);
  // KDE ≥ 5.85 refuses to run a service menu that is not executable.
  write_executable(&kde_path, &shared::linux_kde_service_menu(&exec))?;

  let nemo_dir = data.join("nemo/actions");
  fs::create_dir_all(&nemo_dir).map_err(|e| e.to_string())?;
  fs::write(
    nemo_dir.join(NEMO_ACTION_FILE),
    shared::linux_nemo_action(&exec),
  )
  .map_err(|e| e.to_string())?;

  // Thunar: merge into the user's actions instead of replacing them.
  let thunar_path = thunar_uca_path()?;
  if let Some(dir) = thunar_path.parent() {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
  }
  let existing = fs::read_to_string(&thunar_path).unwrap_or_default();
  fs::write(
    &thunar_path,
    shared::upsert_thunar_action(&existing, &shared::linux_thunar_action(&exec)),
  )
  .map_err(|e| e.to_string())?;

  flush_kde_services();
  log::info!("context menu: registered the Nautilus, Dolphin, Nemo and Thunar entries");
  Ok(())
}

pub fn unregister_context_menu(_app: &tauri::AppHandle) -> Result<(), String> {
  for path in context_menu_files()? {
    if path.exists() {
      fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
  }

  // Thunar's file belongs to the user; take only our action out of it.
  let thunar_path = thunar_uca_path()?;
  if let Ok(existing) = fs::read_to_string(&thunar_path) {
    let updated = shared::remove_thunar_action(&existing, shared::THUNAR_ACTION_ID);
    if updated != existing {
      fs::write(&thunar_path, updated).map_err(|e| e.to_string())?;
    }
  }

  flush_kde_services();
  Ok(())
}

/// Installed when any file manager has the entry. Checking only one of them
/// would report "not installed" to a KDE or XFCE user whose entry is right
/// there in their own file manager.
pub fn is_context_menu_registered(_app: &tauri::AppHandle) -> bool {
  let Ok(files) = context_menu_files() else {
    return false;
  };
  if files.iter().any(|path| path.exists()) {
    return true;
  }
  thunar_uca_path()
    .and_then(|path| fs::read_to_string(path).map_err(|e| e.to_string()))
    .map(|text| text.contains(shared::THUNAR_ACTION_ID))
    .unwrap_or(false)
}

/// The launcher badge — the Linux answer to the macOS dock badge.
///
/// Every launcher that can show a count (KDE's task manager, GNOME's Dash to
/// Dock and Ubuntu Dock, Cinnamon's window list, Plank, Latte, elementary's
/// dock) listens for one D-Bus signal: `com.canonical.Unity.LauncherEntry`'s
/// `Update`. Tauri's own `set_badge_count` reaches that signal only through
/// `libunity`, and only while an actual Unity session is running — which no
/// mainstream desktop has been for a decade, so on its own it is a no-op
/// everywhere and the unread count simply never appeared. Emitting the signal
/// directly is what the launchers actually want.
///
/// `gdbus` ships with GLib, which WebKitGTK already requires, so this adds no
/// dependency. A desktop with no listener ignores the signal.
pub fn set_unread_badge(window: &tauri::WebviewWindow, count: u32) {
  // Still worth doing: on a real Unity session this is the supported path, and
  // it costs nothing where libunity is absent.
  let value = if count == 0 { None } else { Some(count as i64) };
  if let Err(e) = window.set_badge_count(value) {
    log::debug!("badge: libunity did not take the unread count: {e}");
  }

  // The unread count is recomputed on every mail refresh but rarely changes;
  // emitting only on a real change keeps this from spawning a process each time.
  static LAST_COUNT: AtomicI64 = AtomicI64::new(-1);
  if LAST_COUNT.swap(count as i64, Ordering::Relaxed) == count as i64 {
    return;
  }

  if !has_command("gdbus") {
    log::debug!("badge: gdbus is not installed, so the launcher cannot be told");
    return;
  }

  let path = shared::unity_launcher_path(DESKTOP_FILE);
  let properties = shared::unity_launcher_properties(count);
  let signal = format!("{}.Update", shared::UNITY_LAUNCHER_INTERFACE);
  let app_uri = format!("application://{DESKTOP_FILE}");

  let result = Command::new("gdbus")
    .args([
      "emit",
      "--session",
      "--object-path",
      path.as_str(),
      "--signal",
      signal.as_str(),
      app_uri.as_str(),
      properties.as_str(),
    ])
    .output();

  match result {
    Ok(output) if !output.status.success() => log::debug!(
      "badge: the launcher signal was refused: {}",
      String::from_utf8_lossy(&output.stderr).trim()
    ),
    Err(e) => log::debug!("badge: could not emit the launcher signal: {e}"),
    _ => {}
  }
}

pub fn installed_app_path(_app: &tauri::AppHandle) -> Option<PathBuf> {
  if let Some(appimage) = std::env::var_os("APPIMAGE") {
    let path = PathBuf::from(appimage);
    if path.exists() {
      return Some(path);
    }
  }
  std::env::current_exe().ok()
}

/// System prefixes: anything installed here belongs to the package manager, not
/// to us.
fn is_system_path(path: &Path) -> bool {
  const SYSTEM_PREFIXES: [&str; 6] = ["/usr", "/opt", "/bin", "/sbin", "/snap", "/var/lib/flatpak"];
  SYSTEM_PREFIXES
    .iter()
    .any(|prefix| path.starts_with(prefix))
}

/// The command that removes a packaged install on this system.
fn package_removal_command(exe: &Path) -> Option<String> {
  if exe.starts_with("/snap") {
    return Some("sudo snap remove guvercin".to_string());
  }
  if exe.starts_with("/var/lib/flatpak") || exe.starts_with("/app") {
    return Some("flatpak uninstall com.guvercin.app".to_string());
  }
  for (bin, command) in [
    ("apt", "sudo apt remove guvercin"),
    ("dnf", "sudo dnf remove guvercin"),
    ("zypper", "sudo zypper remove guvercin"),
    ("pacman", "sudo pacman -R guvercin"),
    ("dpkg", "sudo dpkg -r guvercin"),
    ("rpm", "sudo rpm -e guvercin"),
  ] {
    if has_command(bin) {
      return Some(command.to_string());
    }
  }
  None
}

/// Everything the app scattered around the user's home to make itself visible
/// to the desktop: its desktop entries (ours, and the one the deep-link plugin
/// writes for the URI schemes), the icon it installed, and the `mimeapps.list`
/// associations pointing at it.
///
/// On macOS all of this lives inside the `.app` bundle and goes with it; on
/// Linux it is spread across `~/.local/share` and `~/.config`, and a leftover
/// default handler pointing at an application that no longer exists leaves the
/// desktop with no working mail client at all — so it is cleaned up explicitly.
fn remove_desktop_integration() {
  if let Ok(dir) = applications_dir() {
    for name in our_desktop_files() {
      let entry = dir.join(&name);
      if entry.exists() {
        let _ = fs::remove_file(&entry);
      }
    }
    let _ = run("update-desktop-database", &[&dir.to_string_lossy()]);
  }

  remove_icon();

  if let Ok(path) = config_home().map(|dir| dir.join("mimeapps.list")) {
    if let Ok(existing) = fs::read_to_string(&path) {
      let mut updated = existing.clone();
      for name in our_desktop_files() {
        updated = shared::strip_mimeapps_entries(&updated, &name);
      }
      if updated != existing {
        let _ = fs::write(&path, updated);
      }
    }
  }
}

/// Removes the desktop integration always, and the application itself when it
/// is ours to remove — an AppImage or a copy under the user's home. A
/// package-managed install is left to the package manager, with the exact
/// command reported back instead of a silent failure.
pub fn remove_installed_app(app: &tauri::AppHandle) -> Result<AppRemoval, String> {
  let _ = unregister_context_menu(app);
  remove_desktop_integration();

  if let Some(appimage) = std::env::var_os("APPIMAGE") {
    let path = PathBuf::from(appimage);
    if path.exists() {
      fs::remove_file(&path)
        .map_err(|e| format!("Could not remove {}: {e}", path.display()))?;
      return Ok(AppRemoval::removed());
    }
  }

  let exe = std::env::current_exe().map_err(|e| e.to_string())?;
  if !is_system_path(&exe) {
    // A self-contained copy (for example under ~/.local/bin): ours to delete.
    // Unlinking a running executable is allowed on Unix.
    fs::remove_file(&exe).map_err(|e| format!("Could not remove {}: {e}", exe.display()))?;
    return Ok(AppRemoval::removed());
  }

  let command = package_removal_command(&exe);
  let hint = command
    .clone()
    .map(|c| format!(" Run: {c}"))
    .unwrap_or_default();
  Ok(AppRemoval::manual(
    format!(
      "guvercin was installed by your package manager, so it has to be removed with it. \
       Your desktop integration has been removed.{hint}"
    ),
    command,
  ))
}
