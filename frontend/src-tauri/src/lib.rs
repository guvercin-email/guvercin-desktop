use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{
  menu::{Menu, MenuItem, PredefinedMenuItem},
  tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
  Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_log::{Target, TargetKind};

mod platform;

/// Event carrying `.eml`/`.msg` files the OS asked us to open while running.
/// (Attachment requests need no event: the compose window is opened here.)
const EVENT_OPEN_FILE: &str = "os://open-file";

/// Label of the primary window. Tauri assigns "main" to the first window
/// declared in tauri.conf.json.
const MAIN_WINDOW_LABEL: &str = "main";
const TRAY_ID: &str = "guvercin-tray";

/// Shared state holding the port the Rust backend is listening on.
/// Set once during app setup; read by the `get_backend_port` command.
#[derive(Default)]
struct BackendPort(Mutex<Option<u16>>);

/// WebKitGTK builds its context menu before the page sees the event, so a
/// `preventDefault()` in the webview does not always suppress it — the guard in
/// `main.jsx` (which is what handles this on macOS and Windows, where WKWebView
/// and WebView2 do respect it) needs this native backstop here. Editable fields
/// keep their menu on every platform, so copy/paste still works.
#[cfg(any(
  target_os = "linux",
  target_os = "dragonfly",
  target_os = "freebsd",
  target_os = "netbsd",
  target_os = "openbsd"
))]
fn disable_native_webview_context_menus(window: &tauri::WebviewWindow) {
  use webkit2gtk::{ContextMenuExt, HitTestResultExt, WebViewExt};

  let _ = window.with_webview(|webview| {
    let wv = webview.inner();
    wv.connect_context_menu(|_, menu, _, hit_test| {
      // Allow native menu for editable fields (copy/paste), disable everywhere else.
      if hit_test.context_is_editable() {
        return false;
      }

      // Extra safety: clear any items that might have been appended by default handlers.
      menu.remove_all();
      true
    });
  });
}

/// Shared state that maps window labels → mail data JSON.
/// The new window calls `get_mail_window_data` to consume its entry.
#[derive(Default)]
struct MailWindowStore(Mutex<HashMap<String, String>>);

/// Shared state that maps window labels → compose data JSON.
#[derive(Default)]
struct ComposeWindowStore(Mutex<HashMap<String, String>>);

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum LinkClickBehavior {
  #[default]
  Ask,
  Open,
  Copy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Preferences {
  #[serde(default)]
  link_click_behavior: LinkClickBehavior,
  #[serde(default)]
  domain_behaviors: HashMap<String, LinkClickBehavior>,
}

impl Default for Preferences {
  fn default() -> Self {
    Self {
      link_click_behavior: LinkClickBehavior::Ask,
      domain_behaviors: HashMap::new(),
    }
  }
}

struct ContextMenuStore {
  registered_path: PathBuf,
}

impl ContextMenuStore {
  fn new(path: PathBuf) -> Self {
    Self {
      registered_path: path,
    }
  }

  fn is_registered(&self) -> bool {
    self.registered_path.exists()
  }

  fn mark_registered(&self) -> Result<(), String> {
    if let Some(parent) = self.registered_path.parent() {
      fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&self.registered_path, "1").map_err(|e| e.to_string())?;
    Ok(())
  }
}

struct PreferencesStore {
  path: PathBuf,
  prefs: Mutex<Preferences>,
}

impl PreferencesStore {
  fn load(path: PathBuf) -> Self {
    let prefs = match fs::read_to_string(&path) {
      Ok(raw) => serde_json::from_str::<Preferences>(&raw).unwrap_or_default(),
      Err(_) => Preferences::default(),
    };
    Self {
      path,
      prefs: Mutex::new(prefs),
    }
  }

  fn set_behavior(&self, behavior: LinkClickBehavior) -> Result<(), String> {
    {
      let mut guard = self.prefs.lock().unwrap();
      guard.link_click_behavior = behavior;
    }
    self.persist()
  }

  fn get_behavior(&self) -> LinkClickBehavior {
    self.prefs.lock().unwrap().link_click_behavior
  }

  fn set_domain_behavior(&self, domain: String, behavior: LinkClickBehavior) -> Result<(), String> {
    {
      let mut guard = self.prefs.lock().unwrap();
      guard.domain_behaviors.insert(domain, behavior);
    }
    self.persist()
  }

  fn get_domain_behavior(&self, domain: &str) -> Option<LinkClickBehavior> {
    self.prefs.lock().unwrap().domain_behaviors.get(domain).copied()
  }

  fn remove_domain_behavior(&self, domain: &str) -> Result<(), String> {
    {
      let mut guard = self.prefs.lock().unwrap();
      guard.domain_behaviors.remove(domain);
    }
    self.persist()
  }

  fn get_all_domain_behaviors(&self) -> HashMap<String, LinkClickBehavior> {
    self.prefs.lock().unwrap().domain_behaviors.clone()
  }

  fn persist(&self) -> Result<(), String> {
    if let Some(parent) = self.path.parent() {
      fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let raw = {
      let guard = self.prefs.lock().unwrap();
      serde_json::to_string_pretty(&*guard).map_err(|e| e.to_string())?
    };
    fs::write(&self.path, raw).map_err(|e| e.to_string())?;
    Ok(())
  }
}

fn parse_behavior(input: &str) -> Option<LinkClickBehavior> {
  match input.trim().to_lowercase().as_str() {
    "ask" => Some(LinkClickBehavior::Ask),
    "open" => Some(LinkClickBehavior::Open),
    "copy" => Some(LinkClickBehavior::Copy),
    _ => None,
  }
}

fn is_allowed_external_url(url: &str) -> bool {
  let u = url.trim();
  u.starts_with("http://")
    || u.starts_with("https://")
    || u.starts_with("mailto:")
    || u.starts_with("tel:")
}

/// Files the OS handed us on the command line, kept until the frontend is up.
///
/// A cold start races the UI: the OS launches us *because* the user opened a
/// message file or picked "Send with guvercin", so the paths arrive before any
/// webview exists to receive an event. They are parked here and drained by the
/// frontend through `take_launch_files` / `take_launch_attachments`; while the
/// app is already running the same paths arrive as events instead.
#[derive(Default)]
struct LaunchQueue {
  files: Mutex<Vec<String>>,
  attachments: Mutex<Vec<String>>,
}

impl LaunchQueue {
  fn push(&self, files: Vec<String>, attachments: Vec<String>) {
    if !files.is_empty() {
      self.files.lock().unwrap().extend(files);
    }
    if !attachments.is_empty() {
      self.attachments.lock().unwrap().extend(attachments);
    }
  }
}

fn is_message_file(path: &str) -> bool {
  let lower = path.trim().to_lowercase();
  lower.ends_with(".eml") || lower.ends_with(".msg")
}

/// Splits a process argument list into `(message files, attachment paths)`.
///
/// Windows and Linux hand both kinds of request over as arguments — Explorer
/// and the file managers run `guvercin --file-attachment <path>`, and opening a
/// `.eml` runs `guvercin <path>`. (macOS delivers both as `file://` deep links
/// instead, which the deep-link plugin surfaces on its own.) URLs are left
/// alone here; they belong to the deep-link plugin on every platform.
///
/// Every path after `--file-attachment` is an attachment, because a file
/// manager expands a multi-file selection into one argument each — and nothing
/// else on the command line can follow that flag.
fn parse_launch_args<I>(args: I) -> (Vec<String>, Vec<String>)
where
  I: IntoIterator<Item = String>,
{
  let mut files = vec![];
  let mut attachments = vec![];
  let mut collecting_attachments = false;

  for arg in args.into_iter().skip(1) {
    if arg == "--file-attachment" {
      collecting_attachments = true;
      continue;
    }
    if let Some(path) = arg.strip_prefix("--file-attachment=") {
      collecting_attachments = true;
      if !path.trim().is_empty() {
        attachments.push(path.to_string());
      }
      continue;
    }
    if arg.starts_with('-') {
      collecting_attachments = false;
      continue;
    }
    if collecting_attachments {
      if !arg.trim().is_empty() {
        attachments.push(arg);
      }
      continue;
    }
    if is_message_file(&arg) {
      files.push(arg);
    }
  }

  (files, attachments)
}

/// Handles a launch (or a second launch forwarded by the single-instance
/// plugin) that carries file arguments. `running` decides whether the paths go
/// out as events or wait in the queue for the frontend to ask for them.
fn handle_launch_args(handle: &tauri::AppHandle, args: Vec<String>, running: bool) {
  let (files, attachments) = parse_launch_args(args);
  if files.is_empty() && attachments.is_empty() {
    return;
  }
  log::info!(
    "launch: {} message file(s), {} attachment(s) from the command line",
    files.len(),
    attachments.len()
  );

  if !running {
    handle.state::<LaunchQueue>().push(files, attachments);
    return;
  }

  if !files.is_empty() {
    let _ = handle.emit(EVENT_OPEN_FILE, files);
  }
  if !attachments.is_empty() {
    let handle = handle.clone();
    tauri::async_runtime::spawn(async move {
      if let Err(e) = attach_files_to_compose(handle, attachments).await {
        log::warn!("compose: could not attach the files from the file manager: {e}");
      }
    });
  }
}

/// Drains the message files the app was launched with.
#[tauri::command]
fn take_launch_files(queue: State<'_, LaunchQueue>) -> Vec<String> {
  let mut files = queue.files.lock().unwrap();
  std::mem::take(&mut *files)
}

/// Drains the attachment paths the app was launched with.
#[tauri::command]
fn take_launch_attachments(queue: State<'_, LaunchQueue>) -> Vec<String> {
  let mut attachments = queue.attachments.lock().unwrap();
  std::mem::take(&mut *attachments)
}

#[tauri::command]
async fn open_mail_window(
  handle: tauri::AppHandle,
  label: String,
  mail_data_json: String,
) -> Result<(), String> {
  let label = if label.trim().is_empty() {
    "mail".to_string()
  } else {
    label
  };

  if let Some(window) = handle.get_webview_window(&label) {
    log::info!("win[{label}]: mail window already open, showing and focusing it");
    window.show().map_err(|e| e.to_string())?;
    let _ = window.set_focus();
    return Ok(());
  }

  log::info!("win[{label}]: opening detached mail window");

  // Store the payload in shared app state so the new window can retrieve it
  // via `get_mail_window_data`. We cannot use localStorage (isolated per webview)
  // or URL query parameters (PathBuf strips them on App protocol).
  {
    let store = handle.state::<MailWindowStore>();
    let mut map = store.0.lock().unwrap();
    map.insert(label.clone(), mail_data_json);
  }

  let init_script = format!(
    "window.__GUV_DETACHED__ = {{ kind: 'mail', label: {} }};",
    serde_json::to_string(&label).unwrap_or_else(|_| "\"\"".to_string())
  );

  WebviewWindowBuilder::new(
    &handle,
    &label,
    WebviewUrl::App(PathBuf::from("index.html")),
  )
  .title("guvercin - Mail")
  .initialization_script(init_script)
  .visible(true)
  // Use a sensible default size for detached mail windows and a comfortable
  // minimum so users don't resize to an unusably small viewport.
  .inner_size(1200.0, 800.0)
  .min_inner_size(900.0, 640.0)
  .build()
  .map_err(|e| e.to_string())
  .map(|window| {
    #[cfg(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    ))]
    disable_native_webview_context_menus(&window);
    #[cfg(not(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    )))]
    let _ = window;
  })?;

  Ok(())
}

/// Called by the new window on startup to fetch its mail data.
#[tauri::command]
fn get_mail_window_data(
  label: String,
  store: State<'_, MailWindowStore>,
) -> Option<String> {
  let map = store.0.lock().unwrap();
  map.get(&label).cloned()
}

#[tauri::command]
fn close_mail_window(handle: tauri::AppHandle, label: String) -> Result<(), String> {
  let label = if label.trim().is_empty() {
    "mail".to_string()
  } else {
    label
  };

  {
    let store = handle.state::<MailWindowStore>();
    let mut map = store.0.lock().unwrap();
    map.remove(&label);
  }

  match handle.get_webview_window(&label) {
    Some(window) => {
      log::info!("win[{label}]: closing detached mail window");
      let _ = window.close();
    }
    None => log::info!("win[{label}]: close requested but no such mail window"),
  }
  Ok(())
}

#[tauri::command]
async fn attach_file_to_compose(
  handle: tauri::AppHandle,
  file_path: String,
) -> Result<(), String> {
  attach_files_to_compose(handle, vec![file_path]).await
}

/// Opens one compose window carrying every given file as an attachment.
///
/// A file manager expands a multi-file selection into one path per argument, so
/// this takes the whole selection at once — one message with all of them
/// attached, rather than one window per file (which the shared window label
/// would collapse into a single attachment anyway).
#[tauri::command]
async fn attach_files_to_compose(
  handle: tauri::AppHandle,
  file_paths: Vec<String>,
) -> Result<(), String> {
  use base64::Engine as _;

  let mut attachments: Vec<Value> = vec![];
  let mut errors: Vec<String> = vec![];

  for file_path in file_paths {
    let path = PathBuf::from(file_path.trim());
    if !path.exists() {
      errors.push(format!("{}: file not found", path.display()));
      continue;
    }

    let file_name = path
      .file_name()
      .and_then(|n| n.to_str())
      .unwrap_or("attachment")
      .to_string();

    let bytes = match fs::read(&path) {
      Ok(bytes) => bytes,
      Err(e) => {
        errors.push(format!("{}: {e}", path.display()));
        continue;
      }
    };
    log::info!(
      "compose: attaching file from the file manager: {} ({} bytes)",
      file_name,
      bytes.len()
    );

    attachments.push(serde_json::json!({
      "name": file_name,
      "data_base64": base64::engine::general_purpose::STANDARD.encode(&bytes),
      "mimeType": ""
    }));
  }

  if attachments.is_empty() {
    return Err(if errors.is_empty() {
      "No file to attach".to_string()
    } else {
      errors.join("; ")
    });
  }
  if !errors.is_empty() {
    log::warn!("compose: some files could not be attached: {}", errors.join("; "));
  }

  let compose_data = serde_json::json!({ "attachments": attachments });

  let compose_result = open_compose_window(
    handle.clone(),
    "compose-with-attachment".to_string(),
    compose_data.to_string(),
  )
  .await;

  // The "Send with guvercin" flow is initiated from the file manager, not the
  // app, so the user wants just the compose window — hide the main window (only
  // the window, not the whole app, so the compose window keeps focus on macOS).
  if let Some(main) = handle.get_webview_window(MAIN_WINDOW_LABEL) {
    log::info!("win[{MAIN_WINDOW_LABEL}]: hiding main window behind the attachment compose window");
    let _ = main.hide();
  }

  compose_result
}

#[tauri::command]
async fn open_compose_window(
  handle: tauri::AppHandle,
  label: String,
  compose_data_json: String,
) -> Result<(), String> {
  let label = if label.trim().is_empty() {
    "compose".to_string()
  } else {
    label
  };

  if let Some(window) = handle.get_webview_window(&label) {
    log::info!("win[{label}]: compose window already open, showing and focusing it");
    window.show().map_err(|e| e.to_string())?;
    let _ = window.set_focus();
    return Ok(());
  }

  log::info!("win[{label}]: opening compose window");

  {
    let store = handle.state::<ComposeWindowStore>();
    let mut map = store.0.lock().unwrap();
    map.insert(label.clone(), compose_data_json);
  }

  let init_script = format!(
    "window.__GUV_DETACHED__ = {{ kind: 'compose', label: {} }};",
    serde_json::to_string(&label).unwrap_or_else(|_| "\"\"".to_string())
  );

  WebviewWindowBuilder::new(
    &handle,
    &label,
    WebviewUrl::App(PathBuf::from("index.html")),
  )
  .title("guvercin - Compose")
  .initialization_script(init_script)
  .visible(true)
  .inner_size(800.0, 650.0)
  .build()
  .map_err(|e| e.to_string())
  .map(|window| {
    #[cfg(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    ))]
    disable_native_webview_context_menus(&window);
    #[cfg(not(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    )))]
    let _ = window;
  })?;

  Ok(())
}

#[tauri::command]
fn get_compose_window_data(
  label: String,
  store: State<'_, ComposeWindowStore>,
) -> Option<String> {
  let map = store.0.lock().unwrap();
  map.get(&label).cloned()
}

#[tauri::command]
fn close_compose_window(handle: tauri::AppHandle, label: String) -> Result<(), String> {
  let label = if label.trim().is_empty() {
    "compose".to_string()
  } else {
    label
  };
  match handle.get_webview_window(&label) {
    Some(window) => {
      log::info!("win[{label}]: closing compose window");
      let _ = window.close();
    }
    None => log::info!("win[{label}]: close requested but no such compose window"),
  }
  Ok(())
}

#[tauri::command]
fn save_export_file_to_path(path: String, bytes: Vec<u8>) -> Result<(), String> {
  let path = PathBuf::from(path);
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
  }

  fs::write(&path, bytes).map_err(|e| e.to_string())?;
  Ok(())
}

#[tauri::command]
fn get_link_click_behavior(store: State<'_, PreferencesStore>) -> String {
  match store.get_behavior() {
    LinkClickBehavior::Ask => "ask".to_string(),
    LinkClickBehavior::Open => "open".to_string(),
    LinkClickBehavior::Copy => "copy".to_string(),
  }
}

#[tauri::command]
fn set_link_click_behavior(
  behavior: String,
  store: State<'_, PreferencesStore>,
) -> Result<(), String> {
  let parsed = parse_behavior(&behavior).ok_or_else(|| "Invalid behavior".to_string())?;
  store.set_behavior(parsed)
}

#[tauri::command]
fn get_domain_link_behavior(domain: String, store: State<'_, PreferencesStore>) -> Option<String> {
  store.get_domain_behavior(&domain).map(|b| match b {
    LinkClickBehavior::Ask => "ask".to_string(),
    LinkClickBehavior::Open => "open".to_string(),
    LinkClickBehavior::Copy => "copy".to_string(),
  })
}

#[tauri::command]
fn set_domain_link_behavior(
  domain: String,
  behavior: String,
  store: State<'_, PreferencesStore>,
) -> Result<(), String> {
  let parsed = parse_behavior(&behavior).ok_or_else(|| "Invalid behavior".to_string())?;
  store.set_domain_behavior(domain, parsed)
}

#[tauri::command]
fn remove_domain_link_behavior(
  domain: String,
  store: State<'_, PreferencesStore>,
) -> Result<(), String> {
  store.remove_domain_behavior(&domain)
}

#[tauri::command]
fn get_all_domain_link_behaviors(
  store: State<'_, PreferencesStore>,
) -> HashMap<String, String> {
  store
    .get_all_domain_behaviors()
    .into_iter()
    .map(|(k, v)| {
      let v_str = match v {
        LinkClickBehavior::Ask => "ask".to_string(),
        LinkClickBehavior::Open => "open".to_string(),
        LinkClickBehavior::Copy => "copy".to_string(),
      };
      (k, v_str)
    })
    .collect()
}

/// Reads a `.eml`/`.msg` file the OS opened us with (via file association) and
/// returns its contents base64-encoded. Accepts either a plain filesystem path
/// or a `file://` URL (macOS delivers file associations as `file://` deep links).
/// Only message file extensions are allowed so this can't be used to read
/// arbitrary files off disk.
#[tauri::command]
fn read_eml_file(path: String) -> Result<String, String> {
  use base64::Engine as _;

  let raw = path.trim();
  // Normalize a `file://` URL into a filesystem path.
  let path_str = if let Some(rest) = raw.strip_prefix("file://") {
    // Drop an optional host component (file://host/path -> /path).
    let rest = match rest.find('/') {
      Some(idx) => &rest[idx..],
      None => rest,
    };
    percent_decode(rest)
  } else {
    raw.to_string()
  };

  let path = PathBuf::from(&path_str);
  let ext = path
    .extension()
    .and_then(|e| e.to_str())
    .map(|e| e.to_ascii_lowercase())
    .unwrap_or_default();
  if ext != "eml" && ext != "msg" {
    return Err("Only .eml or .msg files can be opened".to_string());
  }

  let bytes = fs::read(&path).map_err(|e| e.to_string())?;
  Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// Minimal percent-decoder for `file://` URL paths (handles %20 etc.).
fn percent_decode(input: &str) -> String {
  let bytes = input.as_bytes();
  let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
  let mut i = 0;
  while i < bytes.len() {
    if bytes[i] == b'%' && i + 2 < bytes.len() {
      let hi = (bytes[i + 1] as char).to_digit(16);
      let lo = (bytes[i + 2] as char).to_digit(16);
      if let (Some(hi), Some(lo)) = (hi, lo) {
        out.push((hi * 16 + lo) as u8);
        i += 3;
        continue;
      }
    }
    out.push(bytes[i]);
    i += 1;
  }
  String::from_utf8_lossy(&out).into_owned()
}

#[tauri::command]
fn get_backend_port(state: State<'_, BackendPort>) -> Option<u16> {
  *state.0.lock().unwrap()
}

#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
  if !is_allowed_external_url(&url) {
    return Err("URL scheme not allowed".to_string());
  }
  // Detached: `xdg-open` on Linux keeps running for as long as the browser it
  // started, so waiting on the child would block this command.
  open::that_detached(url).map_err(|e| e.to_string())?;
  Ok(())
}

/// Makes guvercin the OS default handler for `mailto:` links and `.eml` files.
/// Implemented for all three desktop platforms — see `platform` for how each
/// one grants it, and what Windows makes the user confirm themselves.
#[tauri::command]
fn set_as_default_mail_client(
  app: tauri::AppHandle,
) -> Result<platform::DefaultMailOutcome, String> {
  platform::set_as_default_mail_client(&app)
}

#[tauri::command]
fn is_default_mail_client(app: tauri::AppHandle) -> bool {
  platform::is_default_mail_client(&app)
}

/// Installs the file manager's "Send with guvercin" entry (Finder Quick Action,
/// Explorer context menu, Nautilus/Dolphin/Nemo).
#[tauri::command]
fn register_file_context_menu(app: tauri::AppHandle) -> Result<(), String> {
  platform::register_context_menu(&app)
}

#[tauri::command]
fn unregister_file_context_menu(app: tauri::AppHandle) -> Result<(), String> {
  platform::unregister_context_menu(&app)
}

#[tauri::command]
fn is_file_context_menu_registered(app: tauri::AppHandle) -> bool {
  platform::is_context_menu_registered(&app)
}

#[tauri::command]
fn copy_to_clipboard(text: String) -> Result<(), String> {
  // X11 and Wayland have no clipboard server: the *application* owns the
  // selection and serves it to whoever pastes. arboard's `wait()` keeps that
  // ownership alive on a background thread until another app takes over —
  // without it the copied text vanishes the moment the Clipboard is dropped,
  // so copying silently did nothing on Linux while working on macOS/Windows.
  #[cfg(all(unix, not(target_os = "macos")))]
  {
    use arboard::SetExtLinux;

    // `wait()` blocks for as long as we own the selection, so it runs on its
    // own thread; the channel only carries whether the clipboard could be
    // opened at all, which is the one failure the user needs to hear about.
    let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::spawn(move || {
      let mut clipboard = match arboard::Clipboard::new() {
        Ok(clipboard) => {
          let _ = tx.send(Ok(()));
          clipboard
        }
        Err(e) => {
          let _ = tx.send(Err(e.to_string()));
          return;
        }
      };
      if let Err(e) = clipboard.set().wait().text(text) {
        log::warn!("clipboard: stopped serving the copied text: {e}");
      }
    });

    match rx.recv_timeout(std::time::Duration::from_secs(2)) {
      Ok(result) => result,
      // A clipboard that is merely slow to open is not a failure to report.
      Err(_) => Ok(()),
    }
  }
  #[cfg(not(all(unix, not(target_os = "macos"))))]
  {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    clipboard.set_text(text).map_err(|e| e.to_string())?;
    Ok(())
  }
}

/// Brings the main window back from the tray: unhides the app (macOS), then
/// unminimizes, shows and focuses the window.
fn show_main_window(app: &tauri::AppHandle) {
  log::info!("win[{MAIN_WINDOW_LABEL}]: restoring main window");
  // On macOS the window is hidden by hiding the whole application (see the
  // close-to-tray handler), so it must be unhidden before the window can show.
  #[cfg(target_os = "macos")]
  if let Err(e) = app.show() {
    log::warn!("app: unhide failed: {e}");
  }
  match app.get_webview_window(MAIN_WINDOW_LABEL) {
    Some(window) => {
      let _ = window.unminimize();
      if let Err(e) = window.show() {
        log::warn!("win[{MAIN_WINDOW_LABEL}]: show failed: {e}");
      }
      let _ = window.set_focus();
      log::info!(
        "win[{MAIN_WINDOW_LABEL}]: restored (visible={:?})",
        window.is_visible()
      );
    }
    None => log::warn!("win[{MAIN_WINDOW_LABEL}]: cannot restore — main window is gone"),
  }
}

/// Updates the unread-mail indicator: the OS badge (the macOS dock, the Linux
/// launcher, the Windows taskbar overlay) and the tray tooltip. A count of 0
/// clears both.
#[tauri::command]
fn set_unread_badge(app: tauri::AppHandle, count: u32) -> Result<(), String> {
  if let Some(window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
    platform::set_unread_badge(&window, count);
  }

  if let Some(tray) = app.tray_by_id(TRAY_ID) {
    let tooltip = if count == 0 {
      "guvercin".to_string()
    } else {
      format!("guvercin — {count} unread")
    };
    let _ = tray.set_tooltip(Some(tooltip));
  }

  Ok(())
}

fn sanitize_theme_name(input: &str) -> String {
  let mut out = String::new();
  for ch in input.trim().to_lowercase().chars() {
    if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
      out.push(ch);
    } else if ch.is_whitespace() {
      out.push('-');
    }
  }
  while out.contains("--") {
    out = out.replace("--", "-");
  }
  out.trim_matches('-').to_string()
}

fn user_theme_dir(handle: &tauri::AppHandle) -> Result<PathBuf, String> {
  let base = handle
    .path()
    .app_data_dir()
    .map_err(|e| e.to_string())?;
  let dir = base.join("themes").join("user");
  fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
  Ok(dir)
}

fn validate_theme_json(raw: &str) -> Result<Value, String> {
  let mut value: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
  let obj = value.as_object_mut().ok_or_else(|| "Theme JSON must be an object".to_string())?;

  let name = obj
    .get("name")
    .and_then(|v| v.as_str())
    .unwrap_or("")
    .trim();
  if name.is_empty() {
    return Err("Theme JSON missing name".to_string());
  }

  let vars = obj
    .get("vars")
    .and_then(|v| v.as_object())
    .ok_or_else(|| "Theme JSON missing vars".to_string())?;
  if vars.is_empty() {
    return Err("Theme JSON vars is empty".to_string());
  }

  for (k, v) in vars.iter() {
    if !k.starts_with("--") {
      return Err("Theme vars keys must start with --".to_string());
    }
    if !v.is_string() {
      return Err("Theme vars values must be strings".to_string());
    }
  }

  Ok(value)
}

#[tauri::command]
fn list_user_themes(handle: tauri::AppHandle) -> Result<Vec<String>, String> {
  let dir = user_theme_dir(&handle)?;
  let mut out: Vec<String> = vec![];
  for entry in fs::read_dir(dir).map_err(|e| e.to_string())? {
    let entry = entry.map_err(|e| e.to_string())?;
    let path = entry.path();
    if path.extension().and_then(|e| e.to_str()).unwrap_or("") != "json" {
      continue;
    }
    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
      if !stem.trim().is_empty() {
        out.push(stem.to_string());
      }
    }
  }
  out.sort();
  out.dedup();
  Ok(out)
}

#[tauri::command]
fn read_user_theme(handle: tauri::AppHandle, name: String) -> Result<String, String> {
  let safe = sanitize_theme_name(&name);
  if safe.is_empty() {
    return Err("Invalid theme name".to_string());
  }
  let dir = user_theme_dir(&handle)?;
  let path = dir.join(format!("{safe}.json"));
  fs::read_to_string(path).map_err(|e| e.to_string())
}

#[tauri::command]
fn write_user_theme(handle: tauri::AppHandle, name: String, json: String) -> Result<(), String> {
  let safe = sanitize_theme_name(&name);
  if safe.is_empty() {
    return Err("Invalid theme name".to_string());
  }
  let mut value = validate_theme_json(&json)?;

  if let Some(obj) = value.as_object_mut() {
    obj.insert("name".to_string(), Value::String(safe.clone()));
  }

  let dir = user_theme_dir(&handle)?;
  let path = dir.join(format!("{safe}.json"));
  fs::write(path, serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?)
    .map_err(|e| e.to_string())?;
  Ok(())
}

/// Every directory this app writes user data into. Resolved through Tauri's
/// path API rather than hardcoded, because the real locations are derived from
/// the bundle identifier (`com.guvercin.app`) and differ per platform — the
/// previous hardcoded `~/Library/Application Support/Guvercin` never existed,
/// which is why uninstalling used to leave all data behind.
fn user_data_dirs(handle: &tauri::AppHandle) -> Vec<PathBuf> {
  let path = handle.path();
  let mut dirs: Vec<PathBuf> = vec![];
  for dir in [
    path.app_data_dir(),
    path.app_local_data_dir(),
    path.app_config_dir(),
    path.app_cache_dir(),
    path.app_log_dir(),
  ]
  .into_iter()
  .flatten()
  {
    if !dirs.contains(&dir) {
      dirs.push(dir);
    }
  }
  dirs
}

/// Lists the user-data directories that currently exist, so the frontend can
/// tell the user exactly what would be removed before they decide.
#[tauri::command]
fn list_user_data_paths(handle: tauri::AppHandle) -> Vec<String> {
  user_data_dirs(&handle)
    .into_iter()
    .filter(|dir| dir.exists())
    .map(|dir| dir.to_string_lossy().to_string())
    .collect()
}

/// Deletes every user-data directory without touching the installed app.
#[tauri::command]
fn delete_user_data(handle: tauri::AppHandle) -> Result<(), String> {
  let mut failures: Vec<String> = vec![];
  for dir in user_data_dirs(&handle) {
    if !dir.exists() {
      continue;
    }
    if let Err(e) = fs::remove_dir_all(&dir) {
      failures.push(format!("{}: {e}", dir.display()));
    }
  }
  if failures.is_empty() {
    Ok(())
  } else {
    Err(failures.join("; "))
  }
}

/// Where this copy of guvercin is installed — the `.app` bundle on macOS, the
/// install directory on Windows, the AppImage or executable on Linux. Shown to
/// the user before an uninstall.
#[tauri::command]
fn installed_app_location(handle: tauri::AppHandle) -> Option<String> {
  platform::installed_app_path(&handle).map(|path| path.to_string_lossy().to_string())
}

/// Removes the installed application. `delete_data` decides whether the user's
/// local data (accounts, cached mail, settings) goes with it — the caller must
/// ask the user explicitly, since keeping the data lets them reinstall and
/// continue where they left off.
///
/// How the app itself goes away differs per platform (macOS deletes its own
/// bundle, Windows hands over to the installer's uninstaller, a packaged Linux
/// install has to be removed by the package manager), so the answer says what
/// actually happened: when `removed` is false the app stays running and the
/// message explains what is left to do.
#[tauri::command]
fn uninstall_app(
  handle: tauri::AppHandle,
  delete_data: bool,
) -> Result<platform::AppRemoval, String> {
  // Data first: it is the part the user explicitly asked about, and it must not
  // depend on whether the app itself could be removed. A file the running
  // process still holds open (the log, on Windows) can refuse to go — that is
  // worth reporting, but not worth cancelling the uninstall over, since what is
  // left behind goes with the installer's own cleanup.
  let mut leftover_data: Option<String> = None;
  if delete_data {
    if let Err(e) = delete_user_data(handle.clone()) {
      log::warn!("uninstall: some user data could not be removed: {e}");
      leftover_data = Some(e);
    }
  }

  let mut outcome = platform::remove_installed_app(&handle)?;
  if !outcome.removed {
    // The app stays up, so this is the one moment the user can be told.
    if let Some(e) = leftover_data {
      outcome.message = format!(
        "{} Some of your data could not be removed while guvercin was running: {e}",
        outcome.message
      );
    }
    log::info!("uninstall: {}", outcome.message);
    return Ok(outcome);
  }

  let quit_handle = handle.clone();
  std::thread::spawn(move || {
    // Give the frontend a moment to show the result before the window goes.
    std::thread::sleep(std::time::Duration::from_millis(500));
    quit_handle.exit(0);
  });

  Ok(outcome)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
  #[allow(unused_mut)]
  let mut builder = tauri::Builder::default();

  // Single-instance must be registered first so a second launch (e.g. the OS
  // spawning us to handle a `mailto:` link) is forwarded to the already-running
  // instance instead of opening a duplicate. Its `deep-link` feature hands the
  // URL to the deep-link plugin, which the frontend receives via `onOpenUrl`.
  // macOS delivers deep links to the running instance natively, so this is only
  // needed on Windows and Linux.
  #[cfg(any(target_os = "windows", target_os = "linux"))]
  {
    builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
      let handle = app.app_handle().clone();
      // Launching guvercin while it is already running means the user wants it
      // in front — the same thing macOS asks for with its Reopen event.
      show_main_window(&handle);
      // Files from Explorer / the file managers: "Send with guvercin", or a
      // double-clicked .eml.
      handle_launch_args(&handle, argv, true);
    }));
  }

  builder
    .plugin(tauri_plugin_deep_link::init())
    .plugin(tauri_plugin_notification::init())
    .plugin(
      tauri_plugin_log::Builder::new()
        .level(log::LevelFilter::Info)
        .targets([
          Target::new(TargetKind::Stdout),
          Target::new(TargetKind::LogDir { file_name: None }),
          Target::new(TargetKind::Webview),
        ])
        .build(),
    )
    .plugin(tauri_plugin_dialog::init())
    .setup(|app| {
      let _app_handle = app.handle().clone();

      // System tray: keeps the app reachable while its window is hidden in the
      // background (see the close-to-tray handler below). Left-clicking the icon
      // restores the window; the context menu offers quick actions.
      //
      // Linux is the exception: its trays speak StatusNotifierItem, which
      // reports menu activations and nothing else — a left click never reaches
      // `on_tray_icon_event`, so on Linux the click opens the menu (whose first
      // item is "Show guvercin") instead of leaving the icon dead.
      {
        #[cfg(all(unix, not(target_os = "macos")))]
        const MENU_ON_LEFT_CLICK: bool = true;
        #[cfg(not(all(unix, not(target_os = "macos"))))]
        const MENU_ON_LEFT_CLICK: bool = false;

        let show_i = MenuItem::with_id(app, "show", "Show guvercin", true, None::<&str>)?;
        let compose_i = MenuItem::with_id(app, "compose", "New Mail", true, None::<&str>)?;
        let settings_i = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
        let sep = PredefinedMenuItem::separator(app)?;
        let quit_i = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
        let menu = Menu::with_items(app, &[&show_i, &compose_i, &settings_i, &sep, &quit_i])?;

        let mut tray_builder = TrayIconBuilder::with_id(TRAY_ID)
          .tooltip("guvercin")
          .menu(&menu)
          .show_menu_on_left_click(MENU_ON_LEFT_CLICK)
          .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "compose" => {
              show_main_window(app);
              let _ = app.emit("tray://new-mail", ());
            }
            "settings" => {
              show_main_window(app);
              let _ = app.emit("tray://settings", ());
            }
            "quit" => app.exit(0),
            _ => {}
          })
          .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
              button: MouseButton::Left,
              button_state: MouseButtonState::Up,
              ..
            } = event
            {
              show_main_window(tray.app_handle());
            }
          });

        if let Some(icon) = app.default_window_icon().cloned() {
          tray_builder = tray_builder.icon(icon);
        }
        tray_builder.build(app)?;
      }

      // Close-to-background: closing the main window (its close button or Cmd+W)
      // hides it instead of quitting, so background mail sync and notifications
      // keep running. Detached mail and compose windows stay open, matching the
      // platform convention that closing a window is not quitting the app. A
      // real quit is available via the tray menu's "Quit" item; the dock icon,
      // the tray icon or a notification click brings the window back through
      // show_main_window().
      if let Some(main_window) = app.get_webview_window(MAIN_WINDOW_LABEL) {
        let handle_for_close = app.handle().clone();
        main_window.on_window_event(move |event| {
          match event {
            WindowEvent::Focused(focused) => {
              log::debug!("win[{MAIN_WINDOW_LABEL}]: focus changed (focused={focused})")
            }
            WindowEvent::Resized(size) => {
              log::debug!("win[{MAIN_WINDOW_LABEL}]: resized to {}x{}", size.width, size.height)
            }
            WindowEvent::Moved(pos) => {
              log::debug!("win[{MAIN_WINDOW_LABEL}]: moved to {},{}", pos.x, pos.y)
            }
            WindowEvent::Destroyed => log::info!("win[{MAIN_WINDOW_LABEL}]: destroyed"),
            _ => {}
          }
          if let WindowEvent::CloseRequested { api, .. } = event {
            log::info!("win[{MAIN_WINDOW_LABEL}]: close requested (Cmd+W / close button / JS close)");
            // macOS: while other windows (compose / detached mail) are still on
            // screen, only this window goes away. When it is the last one, hide
            // the whole application instead of ordering the window out — an
            // ordered-out window is not restored when the app is unhidden, so
            // hiding at the app level is what lets a dock click, Cmd+Tab or a
            // notification bring the window back.
            #[cfg(target_os = "macos")]
            {
              let others_visible = handle_for_close
                .webview_windows()
                .iter()
                .any(|(label, win)| {
                  label != MAIN_WINDOW_LABEL && win.is_visible().unwrap_or(false)
                });
              if others_visible {
                if let Some(win) = handle_for_close.get_webview_window(MAIN_WINDOW_LABEL) {
                  let _ = win.hide();
                }
              } else {
                let _ = handle_for_close.hide();
              }
            }
            #[cfg(not(target_os = "macos"))]
            {
              if let Some(win) = handle_for_close.get_webview_window(MAIN_WINDOW_LABEL) {
                let _ = win.hide();
              }
            }
            api.prevent_close();
          }
        });
      }

      // Register the configured URI schemes (mailto) with the OS at runtime.
      // Required for development and for Linux/Windows where the scheme isn't
      // otherwise installed; harmless on macOS.
      #[cfg(desktop)]
      {
        use tauri_plugin_deep_link::DeepLinkExt;
        if let Err(e) = app.deep_link().register_all() {
          log::warn!("Failed to register deep-link schemes: {}", e);
        }
      }

      let prefs_path = app
        .path()
        .app_data_dir()
        .ok()
        .map(|dir| dir.join("preferences.json"))
        .unwrap_or_else(|| PathBuf::from("preferences.json"));
      app.manage(PreferencesStore::load(prefs_path));

      // "Send with guvercin" in the file manager, installed once on first
      // launch. Every platform gets it: a Finder Quick Action on macOS, an
      // Explorer context-menu verb on Windows, drop-in files for
      // Nautilus/Dolphin/Nemo on Linux. A failure here is never fatal — the
      // marker is only written when the registration actually succeeded, so a
      // later launch tries again.
      let context_menu_marker = app
        .path()
        .app_data_dir()
        .ok()
        .map(|dir| dir.join(".context_menu_registered"))
        .unwrap_or_else(|| PathBuf::from(".context_menu_registered"));
      let context_menu_store = ContextMenuStore::new(context_menu_marker);

      if !context_menu_store.is_registered() {
        match platform::register_context_menu(app.handle())
          .and_then(|_| context_menu_store.mark_registered())
        {
          Ok(()) => log::info!("context menu: \"Send with guvercin\" is installed"),
          Err(e) => log::warn!("context menu: could not install the entry: {e}"),
        }
      }

      // Files the OS launched us with. On Windows and Linux these arrive as
      // arguments (macOS sends them to the deep-link plugin as file:// URLs),
      // and they arrive before the webview exists — so they wait in the queue
      // until the frontend asks for them.
      handle_launch_args(app.handle(), std::env::args().collect(), false);


      // Get app data directory for database
      let db_dir = app.path().app_data_dir().ok().map(|path| {
        let db_path = path.join("databases");
        let _ = std::fs::create_dir_all(&db_path);
        db_path
      });

      // Spawn backend in a separate thread
      std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
          use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
          
          loop {
            match rust_backend::run(db_dir.clone()).await {
              Ok(port) => {
                // Store the port so the frontend can retrieve it.
                let state = _app_handle.state::<BackendPort>();
                *state.0.lock().unwrap() = Some(port);
                // Keep the runtime alive so the spawned axum server keeps running.
                std::future::pending::<()>().await;
                break;
              }
              Err(rust_backend::error::AppError::KeyringDenied(_)) => {
                log::warn!("Keyring access denied; prompting user to retry or quit");
                let confirmed = _app_handle.dialog()
                  .message("Access to the secure storage was denied. guvercin needs this access to protect your account data.")
                  .title("Keyring Access Required")
                  .kind(MessageDialogKind::Warning)
                  .buttons(MessageDialogButtons::OkCancelCustom("Retry".to_string(), "Quit".to_string()))
                  .blocking_show();
                
                if confirmed {
                    // Retry selected (OkCustom)
                    continue;
                } else {
                    // Quit selected (CancelCustom)
                    _app_handle.exit(0);
                    break;
                }
              }
              Err(e) => {
                log::error!("Backend error: {}", e);
                _app_handle.dialog()
                  .message(format!("The backend failed to start: {}", e))
                  .title("Initialization Error")
                  .kind(MessageDialogKind::Error)
                  .blocking_show();
                _app_handle.exit(1);
                break;
              }
            }
          }
        });
      });

      #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      {
        for (_, window) in app.webview_windows() {
          disable_native_webview_context_menus(&window);
        }
      }

      Ok(())
    })
    .invoke_handler(tauri::generate_handler![
      open_mail_window,
      get_mail_window_data,
      close_mail_window,
      open_compose_window,
      attach_file_to_compose,
      attach_files_to_compose,
      get_compose_window_data,
      close_compose_window,
      save_export_file_to_path,
      get_link_click_behavior,
      set_link_click_behavior,
      get_domain_link_behavior,
      set_domain_link_behavior,
      remove_domain_link_behavior,
      get_all_domain_link_behaviors,
      open_external_url,
      set_as_default_mail_client,
      is_default_mail_client,
      register_file_context_menu,
      unregister_file_context_menu,
      is_file_context_menu_registered,
      copy_to_clipboard,
      set_unread_badge,
      list_user_themes,
      read_user_theme,
      write_user_theme,
      get_backend_port,
      read_eml_file,
      take_launch_files,
      take_launch_attachments,
      uninstall_app,
      installed_app_location,
      list_user_data_paths,
      delete_user_data
    ])
    .manage(MailWindowStore::default())
    .manage(ComposeWindowStore::default())
    .manage(BackendPort::default())
    .manage(LaunchQueue::default())
    .build(tauri::generate_context!())
    .expect("error while building tauri application")
    .run(|app_handle, event| {
      // macOS delivers a Reopen event when the user clicks the dock icon while
      // the app is already running. Because closing the window only hides it
      // (close-to-tray), the window won't come back on its own — so bring it
      // back explicitly here.
      #[cfg(target_os = "macos")]
      if let tauri::RunEvent::Reopen { .. } = event {
        show_main_window(app_handle);
      }
      #[cfg(not(target_os = "macos"))]
      {
        // Windows and Linux get the same "brought to the front again" signal
        // from the single-instance plugin instead (see `run`'s builder above).
        let _ = (app_handle, event);
      }
    });
}

#[cfg(test)]
mod tests {
  use super::*;

  fn args(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
  }

  #[test]
  fn launch_args_pick_up_message_files() {
    let (files, attachments) = parse_launch_args(args(&["guvercin.exe", r"C:\mail\note.EML"]));
    assert_eq!(files, vec![r"C:\mail\note.EML".to_string()]);
    assert!(attachments.is_empty());

    let (files, _) = parse_launch_args(args(&["guvercin", "/home/u/a.msg", "/home/u/b.eml"]));
    assert_eq!(files.len(), 2);
  }

  #[test]
  fn launch_args_pick_up_attachments_in_both_forms() {
    let (files, attachments) =
      parse_launch_args(args(&["guvercin", "--file-attachment", "/tmp/report.pdf"]));
    assert!(files.is_empty());
    assert_eq!(attachments, vec!["/tmp/report.pdf".to_string()]);

    let (_, attachments) = parse_launch_args(args(&["guvercin", "--file-attachment=/tmp/a b.png"]));
    assert_eq!(attachments, vec!["/tmp/a b.png".to_string()]);
  }

  #[test]
  fn launch_args_take_a_whole_multi_file_selection() {
    // Dolphin and Nemo expand a multi-file selection into one argument each.
    let (files, attachments) = parse_launch_args(args(&[
      "guvercin",
      "--file-attachment",
      "/tmp/one.pdf",
      "/tmp/two.png",
      "/tmp/three.eml",
    ]));
    assert!(files.is_empty(), "an attachment is not a message to open");
    assert_eq!(
      attachments,
      vec![
        "/tmp/one.pdf".to_string(),
        "/tmp/two.png".to_string(),
        "/tmp/three.eml".to_string(),
      ]
    );
  }

  #[test]
  fn launch_args_ignore_urls_flags_and_the_program_name() {
    // argv[0] is the program; a .eml-looking program name must not be opened.
    let (files, attachments) = parse_launch_args(args(&["/opt/note.eml"]));
    assert!(files.is_empty() && attachments.is_empty());

    // URLs belong to the deep-link plugin, not to us.
    let (files, attachments) = parse_launch_args(args(&[
      "guvercin",
      "mailto:someone@example.com",
      "guvercin://attach-file?path=%2Ftmp%2Fx",
      "--verbose",
      "note.txt",
    ]));
    assert!(files.is_empty(), "only message files are ours");
    assert!(attachments.is_empty());
  }

  #[test]
  fn launch_args_survive_a_missing_attachment_path() {
    let (files, attachments) = parse_launch_args(args(&["guvercin", "--file-attachment"]));
    assert!(files.is_empty());
    assert!(attachments.is_empty());
  }

  #[test]
  fn external_url_schemes_are_restricted() {
    assert!(is_allowed_external_url("https://example.com"));
    assert!(is_allowed_external_url("mailto:a@example.com"));
    assert!(!is_allowed_external_url("file:///etc/passwd"));
    assert!(!is_allowed_external_url("javascript:alert(1)"));
  }
}
