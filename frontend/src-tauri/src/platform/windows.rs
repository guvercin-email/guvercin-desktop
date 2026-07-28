//! Windows: the registry side of the mail-client and Explorer integration, the
//! taskbar overlay badge, and handing an uninstall to the installer.

use std::path::PathBuf;
use std::process::Command;

use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ};
use winreg::RegKey;

use super::shared;
use super::{AppRemoval, DefaultMailOutcome};

/// ProgID that owns `mailto:` links.
const URL_PROGID: &str = "guvercin.Url.mailto";
/// ProgID that owns `.eml` message files.
const EML_PROGID: &str = "guvercin.Eml";
/// Name we are registered under in `Software\Clients\Mail` and
/// `RegisteredApplications` — this is what Settings lists.
const CLIENT_NAME: &str = "guvercin";
const CLIENT_KEY: &str = r"Software\Clients\Mail\guvercin";
const CAPABILITIES_VALUE: &str = r"Software\Clients\Mail\guvercin\Capabilities";
/// Explorer's per-user "for every file type" context menu.
const SHELL_VERB_KEY: &str = r"Software\Classes\*\shell\GuvercinSend";
/// Where Windows records the association the *user* chose. This — not our own
/// registration — is what decides the default, so it is what we read back.
const MAILTO_USER_CHOICE: &str =
  r"SOFTWARE\Microsoft\Windows\Shell\Associations\UrlAssociations\mailto\UserChoice";

fn current_exe() -> Result<String, String> {
  let exe = std::env::current_exe().map_err(|e| e.to_string())?;
  Ok(exe.to_string_lossy().to_string())
}

fn hkcu() -> RegKey {
  RegKey::predef(HKEY_CURRENT_USER)
}

/// Registers the two ProgIDs plus the `Software\Clients\Mail` capabilities
/// block. Everything lives under HKCU, so no elevation is needed; a per-user
/// registration is enough for Windows to offer guvercin in Settings → Default
/// apps, which is the only place the association can actually be granted.
fn register_application(exe: &str) -> Result<(), String> {
  let classes = hkcu()
    .create_subkey(r"Software\Classes")
    .map_err(|e| e.to_string())?
    .0;

  // mailto: handler.
  {
    let (key, _) = classes.create_subkey(URL_PROGID).map_err(|e| e.to_string())?;
    key.set_value("", &"guvercin mail link").map_err(|e| e.to_string())?;
    // The empty "URL Protocol" value is what marks a ProgID as a URL handler.
    key.set_value("URL Protocol", &"").map_err(|e| e.to_string())?;
    let (icon, _) = key.create_subkey("DefaultIcon").map_err(|e| e.to_string())?;
    icon.set_value("", &format!("\"{exe}\",0")).map_err(|e| e.to_string())?;
    let (cmd, _) = key
      .create_subkey(r"shell\open\command")
      .map_err(|e| e.to_string())?;
    cmd
      .set_value("", &shared::windows_open_command(exe))
      .map_err(|e| e.to_string())?;
  }

  // .eml files.
  {
    let (key, _) = classes.create_subkey(EML_PROGID).map_err(|e| e.to_string())?;
    key.set_value("", &"Email Message").map_err(|e| e.to_string())?;
    let (icon, _) = key.create_subkey("DefaultIcon").map_err(|e| e.to_string())?;
    icon.set_value("", &format!("\"{exe}\",0")).map_err(|e| e.to_string())?;
    let (cmd, _) = key
      .create_subkey(r"shell\open\command")
      .map_err(|e| e.to_string())?;
    cmd
      .set_value("", &shared::windows_open_command(exe))
      .map_err(|e| e.to_string())?;

    // Offer guvercin in "Open with" for .eml without stealing the association.
    let (ext, _) = classes.create_subkey(".eml").map_err(|e| e.to_string())?;
    let (open_with, _) = ext
      .create_subkey(r"OpenWithProgids")
      .map_err(|e| e.to_string())?;
    open_with.set_value(EML_PROGID, &"").map_err(|e| e.to_string())?;
  }

  // The capabilities block Settings reads.
  {
    let (client, _) = hkcu().create_subkey(CLIENT_KEY).map_err(|e| e.to_string())?;
    client.set_value("", &CLIENT_NAME).map_err(|e| e.to_string())?;
    let (cmd, _) = client
      .create_subkey(r"shell\open\command")
      .map_err(|e| e.to_string())?;
    cmd.set_value("", &format!("\"{exe}\"")).map_err(|e| e.to_string())?;

    let (caps, _) = client.create_subkey("Capabilities").map_err(|e| e.to_string())?;
    caps.set_value("ApplicationName", &CLIENT_NAME).map_err(|e| e.to_string())?;
    caps
      .set_value(
        "ApplicationDescription",
        &"Mail, calendar, contacts and tasks — on your desktop",
      )
      .map_err(|e| e.to_string())?;
    caps
      .set_value("ApplicationIcon", &format!("\"{exe}\",0"))
      .map_err(|e| e.to_string())?;

    let (urls, _) = caps.create_subkey("UrlAssociations").map_err(|e| e.to_string())?;
    urls.set_value("mailto", &URL_PROGID).map_err(|e| e.to_string())?;

    let (files, _) = caps.create_subkey("FileAssociations").map_err(|e| e.to_string())?;
    files.set_value(".eml", &EML_PROGID).map_err(|e| e.to_string())?;

    let (start_menu, _) = caps.create_subkey("StartMenu").map_err(|e| e.to_string())?;
    start_menu.set_value("Mail", &CLIENT_NAME).map_err(|e| e.to_string())?;
  }

  // Without this entry the capabilities block above is never looked at.
  let (registered, _) = hkcu()
    .create_subkey(r"Software\RegisteredApplications")
    .map_err(|e| e.to_string())?;
  registered
    .set_value(CLIENT_NAME, &CAPABILITIES_VALUE)
    .map_err(|e| e.to_string())?;

  Ok(())
}

pub fn is_default_mail_client(_app: &tauri::AppHandle) -> bool {
  hkcu()
    .open_subkey_with_flags(MAILTO_USER_CHOICE, KEY_READ)
    .and_then(|key| key.get_value::<String, _>("ProgId"))
    .map(|progid| progid.eq_ignore_ascii_case(URL_PROGID))
    .unwrap_or(false)
}

/// Windows 10 and later deliberately refuse to let an application take an
/// association on its own — `IApplicationAssociationRegistration::SetAppAsDefault`
/// fails for unsigned callers and the hash in `UserChoice` cannot be forged. So
/// this registers guvercin properly and then opens the one place where the user
/// can grant it, reporting that the change is not done yet.
pub fn set_as_default_mail_client(_app: &tauri::AppHandle) -> Result<DefaultMailOutcome, String> {
  let exe = current_exe()?;
  register_application(&exe)?;

  // `registeredAppUser` selects guvercin in the list; older builds ignore the
  // parameter and just open the page, which is still the right place.
  let deep_link = format!("ms-settings:defaultapps?registeredAppUser={CLIENT_NAME}");
  if let Err(e) = open::that_detached(&deep_link) {
    log::warn!("default mail: could not open Settings ({e}); trying the plain page");
    open::that_detached("ms-settings:defaultapps").map_err(|e| e.to_string())?;
  }

  if is_default_mail_client(_app) {
    return Ok(DefaultMailOutcome::done());
  }
  Ok(DefaultMailOutcome::pending(
    "Windows only lets you grant this yourself. guvercin is now registered as a mail app — \
     pick it under \"Email\" in the Default apps page that just opened.",
  ))
}

/// Adds "Send with guvercin" to the Explorer context menu of every file type.
/// Under `HKCU\Software\Classes` rather than `HKEY_CLASSES_ROOT`, which needs
/// administrator rights and silently failed for ordinary users.
pub fn register_context_menu(_app: &tauri::AppHandle) -> Result<(), String> {
  let exe = current_exe()?;
  let (verb, _) = hkcu().create_subkey(SHELL_VERB_KEY).map_err(|e| e.to_string())?;
  verb
    .set_value("", &shared::CONTEXT_MENU_LABEL)
    .map_err(|e| e.to_string())?;
  verb.set_value("Icon", &format!("\"{exe}\",0")).map_err(|e| e.to_string())?;

  let (cmd, _) = verb.create_subkey("command").map_err(|e| e.to_string())?;
  cmd
    .set_value("", &shared::windows_attach_command(&exe))
    .map_err(|e| e.to_string())?;

  // The mail-client registration is what makes the entry point at a known app,
  // so keep the two in step.
  register_application(&exe)?;

  log::info!("context menu: registered the Explorer entry");
  Ok(())
}

pub fn unregister_context_menu(_app: &tauri::AppHandle) -> Result<(), String> {
  // delete_subkey_all removes the `command` child too; a missing key is fine.
  let _ = hkcu().delete_subkey_all(SHELL_VERB_KEY);
  Ok(())
}

pub fn is_context_menu_registered(_app: &tauri::AppHandle) -> bool {
  hkcu()
    .open_subkey_with_flags(format!(r"{SHELL_VERB_KEY}\command"), KEY_READ)
    .and_then(|key| key.get_value::<String, _>(""))
    .map(|cmd| cmd.contains("--file-attachment"))
    .unwrap_or(false)
}

/// Windows has no numeric taskbar badge, so the count is drawn as the taskbar
/// overlay icon — the same slot Mail and Teams use.
pub fn set_unread_badge(window: &tauri::WebviewWindow, count: u32) {
  const BADGE_SIZE: u32 = 32;
  let icon = shared::badge_rgba(count, BADGE_SIZE)
    .map(|rgba| tauri::image::Image::new_owned(rgba, BADGE_SIZE, BADGE_SIZE));
  if let Err(e) = window.set_overlay_icon(icon) {
    log::warn!("badge: could not set the taskbar overlay icon: {e}");
  }
}

pub fn installed_app_path(_app: &tauri::AppHandle) -> Option<PathBuf> {
  std::env::current_exe()
    .ok()
    .and_then(|exe| exe.parent().map(|p| p.to_path_buf()))
}

/// Looks for the installer's own uninstall entry, in every place Windows keeps
/// them: per-user and per-machine, 64-bit and 32-bit views.
fn find_uninstall_string() -> Option<String> {
  const ROOTS: [&str; 2] = [
    r"Software\Microsoft\Windows\CurrentVersion\Uninstall",
    r"Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall",
  ];

  for hive in [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE] {
    let root_key = RegKey::predef(hive);
    for root in ROOTS {
      let Ok(uninstall) = root_key.open_subkey_with_flags(root, KEY_READ) else {
        continue;
      };
      let Ok(keys) = uninstall.enum_keys().collect::<Result<Vec<_>, _>>() else {
        continue;
      };
      for name in keys {
        let Ok(entry) = uninstall.open_subkey_with_flags(&name, KEY_READ) else {
          continue;
        };
        let display: String = entry.get_value("DisplayName").unwrap_or_default();
        let is_ours = name.to_lowercase().contains("guvercin")
          || display.to_lowercase().contains("guvercin");
        if !is_ours {
          continue;
        }
        // The quiet variant, when the installer offers one, avoids a second
        // confirmation the user has already given us.
        if let Ok(quiet) = entry.get_value::<String, _>("QuietUninstallString") {
          if !quiet.trim().is_empty() {
            return Some(quiet);
          }
        }
        if let Ok(normal) = entry.get_value::<String, _>("UninstallString") {
          if !normal.trim().is_empty() {
            return Some(normal);
          }
        }
      }
    }
  }
  None
}

/// A running executable cannot delete itself on Windows, so the installer's
/// uninstaller does it: it copies itself to a temporary directory and removes
/// the install directory once we exit.
pub fn remove_installed_app(_app: &tauri::AppHandle) -> Result<AppRemoval, String> {
  // Leave nothing pointing at an app that is going away.
  let _ = unregister_context_menu(_app);
  let _ = hkcu().delete_subkey_all(CLIENT_KEY);
  let _ = hkcu()
    .open_subkey_with_flags(r"Software\RegisteredApplications", winreg::enums::KEY_ALL_ACCESS)
    .map(|key| key.delete_value(CLIENT_NAME));
  let _ = hkcu().delete_subkey_all(format!(r"Software\Classes\{URL_PROGID}"));
  let _ = hkcu().delete_subkey_all(format!(r"Software\Classes\{EML_PROGID}"));

  let Some(raw) = find_uninstall_string() else {
    let location = installed_app_path(_app)
      .map(|p| p.display().to_string())
      .unwrap_or_else(|| "the folder guvercin runs from".to_string());
    return Ok(AppRemoval::manual(
      format!(
        "This copy of guvercin was not installed by the installer, so there is no uninstaller \
         to run. Your settings have been undone; delete {location} to finish."
      ),
      None,
    ));
  };

  let Some((program, args)) = shared::split_uninstall_command(&raw) else {
    return Err(format!("The uninstaller entry could not be read: {raw}"));
  };

  Command::new(&program)
    .args(&args)
    .spawn()
    .map_err(|e| format!("Could not start the uninstaller ({program}): {e}"))?;

  Ok(AppRemoval::removed())
}
