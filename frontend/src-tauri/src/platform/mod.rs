//! OS integration, at parity across macOS, Windows and Linux.
//!
//! Everything the app asks of the operating system beyond drawing a window —
//! becoming the default mail client, putting a "Send with guvercin" entry in
//! the file manager, showing the unread count on the dock/taskbar/launcher, and
//! uninstalling itself — is declared here and implemented for all three
//! platforms. Each capability used to exist on one platform only, which is why
//! this module states the contract once and dispatches: a capability that is
//! genuinely impossible on a platform reports *why* instead of silently doing
//! nothing.
//!
//! [`shared`] holds the platform-neutral halves (file contents, command
//! strings, the badge bitmap) so they are unit-tested on every host; the
//! `macos`, `windows` and `unix` modules hold only the OS-specific calls.

use serde::Serialize;

pub mod shared;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as imp;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
use windows as imp;

// Linux and the BSDs share the freedesktop.org mechanisms (XDG desktop entries,
// xdg-mime, the same file managers), so one implementation serves them all.
#[cfg(all(unix, not(target_os = "macos")))]
mod unix;
#[cfg(all(unix, not(target_os = "macos")))]
use unix as imp;

/// Result of asking the OS to make guvercin the default mail client.
///
/// Windows (10 and later) does not let an application take an association
/// programmatically — it can only register itself and send the user to
/// Settings. `needs_user_action` says so honestly rather than reporting a
/// success that never happened.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DefaultMailOutcome {
  /// Whether guvercin holds the `mailto:` association now.
  pub is_default: bool,
  /// Whether the user still has to confirm the change in an OS dialog.
  pub needs_user_action: bool,
  /// What to tell the user; empty when the change simply took effect.
  pub message: String,
}

// `pending` is only reachable on the platforms that need a confirmation step,
// and `manual` only on those where something can be left for the user to do.
#[allow(dead_code)]
impl DefaultMailOutcome {
  fn done() -> Self {
    Self {
      is_default: true,
      needs_user_action: false,
      message: String::new(),
    }
  }

  fn pending(message: impl Into<String>) -> Self {
    Self {
      is_default: false,
      needs_user_action: true,
      message: message.into(),
    }
  }
}

/// Result of removing the installed application.
///
/// A package-managed Linux install (`.deb`/`.rpm`) cannot remove itself: the
/// files belong to root and to the package database. Rather than pretending,
/// that case comes back with `removed: false` and the exact command to run.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRemoval {
  /// Whether the application itself is being removed (the app should quit).
  pub removed: bool,
  /// What to tell the user when `removed` is false.
  pub message: String,
  /// A command that finishes the removal, when one exists.
  pub command: Option<String>,
}

#[allow(dead_code)]
impl AppRemoval {
  fn removed() -> Self {
    Self {
      removed: true,
      message: String::new(),
      command: None,
    }
  }

  fn manual(message: impl Into<String>, command: Option<String>) -> Self {
    Self {
      removed: false,
      message: message.into(),
      command,
    }
  }
}

/// Whether guvercin currently owns the `mailto:` association.
pub fn is_default_mail_client(app: &tauri::AppHandle) -> bool {
  imp::is_default_mail_client(app)
}

/// Asks the OS to make guvercin the default mail client.
pub fn set_as_default_mail_client(app: &tauri::AppHandle) -> Result<DefaultMailOutcome, String> {
  imp::set_as_default_mail_client(app)
}

/// Installs the file manager's "Send with guvercin" entry.
pub fn register_context_menu(app: &tauri::AppHandle) -> Result<(), String> {
  imp::register_context_menu(app)
}

/// Removes the file manager's "Send with guvercin" entry. Best effort: a
/// missing entry is not an error.
pub fn unregister_context_menu(app: &tauri::AppHandle) -> Result<(), String> {
  imp::unregister_context_menu(app)
}

/// Whether the file manager entry is currently installed.
pub fn is_context_menu_registered(app: &tauri::AppHandle) -> bool {
  imp::is_context_menu_registered(app)
}

/// Shows `count` unread on the dock (macOS), the launcher (Linux — over the
/// D-Bus interface every dock and task manager listens on) or the taskbar icon
/// (Windows). A count of 0 clears it.
pub fn set_unread_badge(window: &tauri::WebviewWindow, count: u32) {
  imp::set_unread_badge(window, count)
}

/// Removes the installed application. Never touches user data — the caller
/// decides that separately.
pub fn remove_installed_app(app: &tauri::AppHandle) -> Result<AppRemoval, String> {
  imp::remove_installed_app(app)
}

/// The application's own install location, shown to the user before an
/// uninstall so they can see what is about to be removed.
pub fn installed_app_path(app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
  imp::installed_app_path(app)
}

// ---------------------------------------------------------------------------
// Fallback for targets that are neither macOS, Windows nor a unix desktop.
// Keeps the crate compiling everywhere; every capability reports honestly that
// the platform has no mechanism for it.
// ---------------------------------------------------------------------------
#[cfg(not(any(unix, target_os = "windows")))]
mod imp {
  use super::{AppRemoval, DefaultMailOutcome};

  const UNSUPPORTED: &str = "This platform has no mechanism for this.";

  pub fn is_default_mail_client(_app: &tauri::AppHandle) -> bool {
    false
  }

  pub fn set_as_default_mail_client(
    _app: &tauri::AppHandle,
  ) -> Result<DefaultMailOutcome, String> {
    Err(UNSUPPORTED.to_string())
  }

  pub fn register_context_menu(_app: &tauri::AppHandle) -> Result<(), String> {
    Ok(())
  }

  pub fn unregister_context_menu(_app: &tauri::AppHandle) -> Result<(), String> {
    Ok(())
  }

  pub fn is_context_menu_registered(_app: &tauri::AppHandle) -> bool {
    false
  }

  pub fn set_unread_badge(window: &tauri::WebviewWindow, count: u32) {
    let _ = window.set_badge_count(if count == 0 { None } else { Some(count as i64) });
  }

  pub fn remove_installed_app(_app: &tauri::AppHandle) -> Result<AppRemoval, String> {
    Err(UNSUPPORTED.to_string())
  }

  pub fn installed_app_path(_app: &tauri::AppHandle) -> Option<std::path::PathBuf> {
    None
  }
}
