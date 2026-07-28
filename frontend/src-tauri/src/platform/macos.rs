//! macOS: LaunchServices associations, the Finder Quick Action, the dock badge
//! and removing the application bundle.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use super::shared;
use super::{AppRemoval, DefaultMailOutcome};

/// Where user-installed Services (Quick Actions) live.
fn services_dir() -> Result<PathBuf, String> {
  let home = std::env::var("HOME").map_err(|_| "HOME is not set".to_string())?;
  Ok(PathBuf::from(home).join("Library/Services"))
}

fn workflow_dir() -> Result<PathBuf, String> {
  Ok(services_dir()?.join(format!("{}.workflow", shared::CONTEXT_MENU_LABEL)))
}

/// Asks the pasteboard server to re-scan Services so the item shows up without
/// a log-out. Missing on trimmed-down systems, so failure is ignored.
fn flush_services() {
  let _ = Command::new("/System/Library/CoreServices/pbs")
    .arg("-flush")
    .status();
}

/// Registers this app as the OS default handler for `mailto:` and `.eml`.
/// LaunchServices is the same mechanism System Settings' "Default email
/// reader" and every other mail app uses.
fn set_default_handlers(bundle_id: &str) -> Result<(), String> {
  use core_foundation::base::TCFType;
  use core_foundation::string::{CFString, CFStringRef};

  extern "C" {
    fn LSSetDefaultHandlerForURLScheme(
      in_url_scheme: CFStringRef,
      in_handler_bundle_id: CFStringRef,
    ) -> i32;
    fn LSSetDefaultRoleHandlerForContentType(
      in_content_type: CFStringRef,
      in_role: u32,
      in_handler_bundle_id: CFStringRef,
    ) -> i32;
  }

  let bundle = CFString::new(bundle_id);

  let scheme = CFString::new("mailto");
  let status = unsafe {
    LSSetDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef(), bundle.as_concrete_TypeRef())
  };
  if status != 0 {
    return Err(format!(
      "LSSetDefaultHandlerForURLScheme failed with status {status}"
    ));
  }

  // Also claim `.eml` files. macOS maps them to the `com.apple.mail.email`
  // content type; kLSRolesAll = 0xFFFFFFFF. Requires the bundle to declare this
  // UTI via LSItemContentTypes (see src-tauri/Info.plist).
  let eml_uti = CFString::new("com.apple.mail.email");
  let eml_status = unsafe {
    LSSetDefaultRoleHandlerForContentType(
      eml_uti.as_concrete_TypeRef(),
      0xFFFF_FFFF,
      bundle.as_concrete_TypeRef(),
    )
  };
  if eml_status != 0 {
    return Err(format!(
      "LSSetDefaultRoleHandlerForContentType failed with status {eml_status}"
    ));
  }

  Ok(())
}

fn default_mailto_handler() -> Option<String> {
  use core_foundation::base::TCFType;
  use core_foundation::string::{CFString, CFStringRef};

  extern "C" {
    fn LSCopyDefaultHandlerForURLScheme(in_url_scheme: CFStringRef) -> CFStringRef;
  }

  let scheme = CFString::new("mailto");
  let handler_ref = unsafe { LSCopyDefaultHandlerForURLScheme(scheme.as_concrete_TypeRef()) };
  if handler_ref.is_null() {
    return None;
  }
  let handler = unsafe { CFString::wrap_under_create_rule(handler_ref) };
  Some(handler.to_string())
}

pub fn is_default_mail_client(app: &tauri::AppHandle) -> bool {
  let id = app.config().identifier.clone();
  default_mailto_handler()
    .map(|handler| handler.eq_ignore_ascii_case(&id))
    .unwrap_or(false)
}

pub fn set_as_default_mail_client(app: &tauri::AppHandle) -> Result<DefaultMailOutcome, String> {
  let id = app.config().identifier.clone();
  set_default_handlers(&id)?;
  Ok(DefaultMailOutcome::done())
}

/// Builds the "Send with guvercin" Quick Action in `~/Library/Services`.
///
/// A Quick Action is an Automator document, not an app bundle: it needs
/// `Contents/Info.plist` *and* `Contents/document.wflow`, and no executable. A
/// half-written bundle is reported by macOS as damaged, so the old one is
/// removed first and the files are written in one go.
pub fn register_context_menu(_app: &tauri::AppHandle) -> Result<(), String> {
  let dir = workflow_dir()?;
  let contents = dir.join("Contents");

  if dir.exists() {
    fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
  }
  fs::create_dir_all(&contents).map_err(|e| e.to_string())?;

  fs::write(
    contents.join("Info.plist"),
    shared::macos_service_info_plist(),
  )
  .map_err(|e| e.to_string())?;
  fs::write(
    contents.join("document.wflow"),
    shared::macos_service_workflow(shared::MACOS_SERVICE_SHELL),
  )
  .map_err(|e| e.to_string())?;

  flush_services();
  log::info!("context menu: registered the macOS Quick Action");
  Ok(())
}

pub fn unregister_context_menu(_app: &tauri::AppHandle) -> Result<(), String> {
  let dir = workflow_dir()?;
  if dir.exists() {
    fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    flush_services();
  }
  Ok(())
}

pub fn is_context_menu_registered(_app: &tauri::AppHandle) -> bool {
  workflow_dir()
    .map(|dir| dir.join("Contents/document.wflow").exists())
    .unwrap_or(false)
}

/// The dock badge. macOS takes the number directly.
pub fn set_unread_badge(window: &tauri::WebviewWindow, count: u32) {
  let value = if count == 0 { None } else { Some(count as i64) };
  if let Err(e) = window.set_badge_count(value) {
    log::warn!("badge: could not set the dock badge: {e}");
  }
}

/// …/guvercin.app/Contents/MacOS/guvercin → …/guvercin.app
pub fn installed_app_path(_app: &tauri::AppHandle) -> Option<PathBuf> {
  let exe = std::env::current_exe().ok()?;
  let bundle = exe.parent()?.parent()?.parent()?;
  if bundle.extension().and_then(|e| e.to_str()) == Some("app") {
    return Some(bundle.to_path_buf());
  }
  None
}

/// Removes the `.app` bundle. macOS lets a running application delete its own
/// bundle (the mapped pages stay valid until it exits), so this is the whole
/// uninstall — there is no installer database to update.
pub fn remove_installed_app(app: &tauri::AppHandle) -> Result<AppRemoval, String> {
  let Some(path) = installed_app_path(app) else {
    return Ok(AppRemoval::manual(
      "guvercin is not running from an application bundle, so there is nothing to remove. \
       Drag the app to the Trash to finish.",
      None,
    ));
  };

  fs::remove_dir_all(&path).map_err(|e| {
    format!(
      "Could not remove {}: {e}. Drag the app to the Trash to finish.",
      path.display()
    )
  })?;
  Ok(AppRemoval::removed())
}
