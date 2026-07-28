//! The platform-neutral half of the OS integration.
//!
//! File contents, registry/command strings and the badge bitmap are built here
//! so the same rules apply on every OS and so they can be unit-tested on any
//! host — the per-platform modules only carry the calls that genuinely differ.

// Every item here is used by at least one platform and covered by the tests
// below, but no single host uses all of them, so on any given build the rest
// would read as dead code.
#![allow(dead_code)]

/// Menu label used by every file manager integration ("Send with guvercin").
pub const CONTEXT_MENU_LABEL: &str = "Send with guvercin";

/// URI the file-manager entries hand back to the app, followed by one
/// percent-encoded `path=` parameter per selected file — a whole selection
/// becomes one message with everything attached. `attachmentInbox.js` parses
/// exactly this shape.
pub const ATTACH_URI_BASE: &str = "guvercin://attach-file?";

/// Escapes the five XML metacharacters so a value can be embedded in a plist.
pub fn xml_escape(input: &str) -> String {
  let mut out = String::with_capacity(input.len());
  for ch in input.chars() {
    match ch {
      '&' => out.push_str("&amp;"),
      '<' => out.push_str("&lt;"),
      '>' => out.push_str("&gt;"),
      '"' => out.push_str("&quot;"),
      '\'' => out.push_str("&apos;"),
      _ => out.push(ch),
    }
  }
  out
}

// ---------------------------------------------------------------------------
// Linux / BSD: desktop entry and file-manager integration
// ---------------------------------------------------------------------------

/// The desktop entry that makes guvercin selectable as the mail handler and
/// carries the `.eml` and `mailto:` associations. `exec` must already be an
/// absolute path to the executable (or the AppImage).
///
/// `wm_class` is the `WM_CLASS` the running window reports — GTK derives it
/// from the program name, so it is the executable's file stem, which is *not*
/// the same as `exec` inside an AppImage. Declaring it matters more than it
/// looks: a desktop that cannot match the window to this entry treats the
/// running app as an unknown application, giving it a second, blank icon in the
/// dock and sending the launcher badge to the entry rather than the window. A
/// macOS bundle identifies itself, so none of this arises there.
pub fn linux_desktop_entry(exec: &str, product_name: &str, wm_class: &str) -> String {
  format!(
    "[Desktop Entry]\n\
     Type=Application\n\
     Version=1.0\n\
     Name={product_name}\n\
     GenericName=Mail Client\n\
     Comment=Mail, calendar, contacts and tasks — on your desktop\n\
     Keywords=Email;E-mail;Mail;Calendar;Contacts;Tasks;IMAP;SMTP;\n\
     Exec=\"{exec}\" %U\n\
     Icon={product_name}\n\
     Terminal=false\n\
     StartupNotify=true\n\
     StartupWMClass={wm_class}\n\
     Categories=Office;Network;Email;\n\
     MimeType=x-scheme-handler/mailto;x-scheme-handler/guvercin;message/rfc822;\n\
     X-GNOME-UsesNotifications=true\n"
  )
}

/// GNOME Files (Nautilus) drops executables in `~/.local/share/nautilus/scripts`
/// into the right-click menu, one menu entry per file name.
pub fn linux_nautilus_script() -> String {
  // NAUTILUS_SCRIPT_SELECTED_FILE_PATHS is newline-separated, so it is read line
  // by line — splitting on whitespace loses every path containing a space. The
  // loop reads from a here-document rather than a pipe so the query it builds
  // survives the loop (a piped `while` runs in a subshell). Each path is
  // percent-encoded byte by byte with `od`, which keeps non-ASCII names intact.
  format!(
    "#!/bin/sh\n\
     # Hands the selected files to guvercin. Installed by guvercin itself.\n\
     query=''\n\
     while IFS= read -r file; do\n\
     \x20 [ -n \"$file\" ] || continue\n\
     \x20 encoded=$(printf '%s' \"$file\" | od -An -tx1 -v | tr -d ' \\n' | sed 's/../%&/g')\n\
     \x20 if [ -z \"$query\" ]; then query=\"path=$encoded\"; else query=\"$query&path=$encoded\"; fi\n\
     done <<GUVERCIN_EOF\n\
     $NAUTILUS_SCRIPT_SELECTED_FILE_PATHS\n\
     GUVERCIN_EOF\n\
     [ -n \"$query\" ] && xdg-open \"{ATTACH_URI_BASE}$query\"\n"
  )
}

/// KDE (Dolphin) service menu. Modern KDE reads
/// `~/.local/share/kio/servicemenus/*.desktop`.
pub fn linux_kde_service_menu(exec: &str) -> String {
  format!(
    "[Desktop Entry]\n\
     Type=Service\n\
     ServiceTypes=KonqPopupMenu/Plugin\n\
     MimeType=all/all;\n\
     Actions=SendWithGuvercin\n\
     X-KDE-Priority=TopLevel\n\
     \n\
     [Desktop Action SendWithGuvercin]\n\
     Name={CONTEXT_MENU_LABEL}\n\
     Icon=mail-message-new\n\
     Exec=\"{exec}\" --file-attachment %F\n"
  )
}

/// Cinnamon (Nemo) action file, `~/.local/share/nemo/actions/*.nemo_action`.
pub fn linux_nemo_action(exec: &str) -> String {
  format!(
    "[Nemo Action]\n\
     Name={CONTEXT_MENU_LABEL}\n\
     Comment=Attach the selected file to a new guvercin message\n\
     Exec=\"{exec}\" --file-attachment %F\n\
     Icon-Name=mail-message-new\n\
     Selection=Any\n\
     Extensions=any;\n\
     Quote=double\n"
  )
}

/// Thunar (XFCE) keeps every custom action in one shared file rather than a
/// drop-in directory, so ours is identified by this id and merged in and out of
/// the user's own actions.
pub const THUNAR_ACTION_ID: &str = "guvercin-attach";

/// Our `<action>` element for Thunar's `~/.config/Thunar/uca.xml`. `%F` is the
/// whole selection, so one message carries every selected file — the same shape
/// as the Dolphin and Nemo entries.
pub fn linux_thunar_action(exec: &str) -> String {
  format!(
    "<action>\n\
     \x20 <icon>mail-message-new</icon>\n\
     \x20 <name>{label}</name>\n\
     \x20 <unique-id>{THUNAR_ACTION_ID}</unique-id>\n\
     \x20 <command>&quot;{exec}&quot; --file-attachment %F</command>\n\
     \x20 <description>Attach the selected files to a new guvercin message</description>\n\
     \x20 <patterns>*</patterns>\n\
     \x20 <audio-files/>\n\
     \x20 <image-files/>\n\
     \x20 <other-files/>\n\
     \x20 <text-files/>\n\
     \x20 <video-files/>\n\
     </action>\n",
    label = xml_escape(CONTEXT_MENU_LABEL),
    exec = xml_escape(exec),
  )
}

const THUNAR_UCA_HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n";

/// Drops the `<action>` block carrying `unique_id`, leaving every other action —
/// and the user's own formatting — untouched. Returns the file unchanged when
/// the action is not in it.
pub fn remove_thunar_action(existing: &str, unique_id: &str) -> String {
  let marker = format!("<unique-id>{unique_id}</unique-id>");
  let mut out = existing.to_string();

  // Each pass removes one `<action>…</action>` containing the marker. A file
  // written by an older version could hold more than one.
  while let Some(marker_at) = out.find(&marker) {
    let Some(start) = out[..marker_at].rfind("<action>") else {
      break;
    };
    let Some(end_at) = out[marker_at..].find("</action>") else {
      break;
    };
    let mut end = marker_at + end_at + "</action>".len();
    // Take the newline the element sat on with it, so removing an action does
    // not leave a blank line behind.
    if out[end..].starts_with('\n') {
      end += 1;
    }
    out.replace_range(start..end, "");
  }
  out
}

/// Puts our action into Thunar's `uca.xml`, replacing an earlier copy of it and
/// keeping every action the user added themselves. An absent or unreadable file
/// is replaced by a minimal valid one.
pub fn upsert_thunar_action(existing: &str, action: &str) -> String {
  let cleaned = remove_thunar_action(existing, THUNAR_ACTION_ID);
  let trimmed = cleaned.trim();

  // No `<actions>` root to merge into (a new or corrupted file): start over.
  let Some(close_at) = trimmed.rfind("</actions>") else {
    return format!("{THUNAR_UCA_HEADER}{action}</actions>\n");
  };

  let mut out = String::with_capacity(trimmed.len() + action.len() + 1);
  out.push_str(&trimmed[..close_at]);
  if !out.ends_with('\n') {
    out.push('\n');
  }
  out.push_str(action);
  out.push_str(&trimmed[close_at..]);
  if !out.ends_with('\n') {
    out.push('\n');
  }
  out
}

// ---------------------------------------------------------------------------
// Linux / BSD: the launcher badge
// ---------------------------------------------------------------------------

/// D-Bus interface every Linux launcher that can show a count listens on —
/// KDE's task manager, GNOME's Dash to Dock, Cinnamon's window list, Plank,
/// Latte and the rest. Unity defined it; the name stuck.
pub const UNITY_LAUNCHER_INTERFACE: &str = "com.canonical.Unity.LauncherEntry";

/// Object path the `Update` signal is emitted from. The listeners key off the
/// `application://…` URI in the signal body rather than the path, so any stable
/// path works; this is the conventional shape.
pub fn unity_launcher_path(desktop_file: &str) -> String {
  // A small stable hash keeps the path constant across runs without carrying
  // characters the D-Bus path grammar forbids.
  let mut hash: u32 = 2_166_136_261;
  for byte in desktop_file.as_bytes() {
    hash ^= *byte as u32;
    hash = hash.wrapping_mul(16_777_619);
  }
  format!("/com/canonical/unity/launcherentry/{hash}")
}

/// The `a{sv}` body of the `Update` signal, in `gdbus`' variant syntax:
/// the count and whether to show it. A count of 0 hides the badge.
pub fn unity_launcher_properties(count: u32) -> String {
  let visible = if count == 0 { "false" } else { "true" };
  format!("{{'count': <int64 {count}>, 'count-visible': <{visible}>}}")
}

/// True when the handler `xdg-settings`/`xdg-mime` reported is one of ours.
/// Both the entry we install and the one the deep-link plugin writes count.
pub fn desktop_handler_matches(output: &str, candidates: &[String]) -> bool {
  let reported = output.trim();
  if reported.is_empty() {
    return false;
  }
  // xdg-mime can report several handlers, whitespace- or newline-separated.
  reported
    .split_whitespace()
    .any(|entry| candidates.iter().any(|c| c == entry))
}

/// Section of `mimeapps.list` that records the user's chosen handlers. This is
/// the file `xdg-mime` and `xdg-settings` write, and the one desktops read.
const MIMEAPPS_SECTION: &str = "[Default Applications]";

/// Reads the desktop entry currently registered for `mime` in a `mimeapps.list`.
pub fn mimeapps_default_for(existing: &str, mime: &str) -> Option<String> {
  let mut in_section = false;
  for line in existing.lines() {
    let trimmed = line.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
      in_section = trimmed == MIMEAPPS_SECTION;
      continue;
    }
    if !in_section {
      continue;
    }
    let Some((key, value)) = trimmed.split_once('=') else {
      continue;
    };
    if key.trim() == mime {
      // A handler list is semicolon-separated; the first entry is the default.
      return value
        .split(';')
        .map(str::trim)
        .find(|entry| !entry.is_empty())
        .map(|entry| entry.to_string());
    }
  }
  None
}

/// Sets `desktop_file` as the handler for every mime type in `mimes`, keeping
/// the rest of the file — other sections, other associations, comments — as it
/// was. Used only when neither `xdg-mime` nor `xdg-settings` is installed.
pub fn upsert_mimeapps_defaults(existing: &str, desktop_file: &str, mimes: &[&str]) -> String {
  let entries: Vec<String> = mimes
    .iter()
    .map(|mime| format!("{mime}={desktop_file}"))
    .collect();

  let mut out: Vec<String> = Vec::new();
  let mut in_section = false;
  let mut written = false;

  for line in existing.lines() {
    let trimmed = line.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
      if in_section {
        out.extend(entries.iter().cloned());
        written = true;
        in_section = false;
      }
      if trimmed == MIMEAPPS_SECTION {
        in_section = true;
      }
      out.push(line.to_string());
      continue;
    }
    // Drop the associations we are about to rewrite, keep everything else.
    if in_section {
      if let Some((key, _)) = trimmed.split_once('=') {
        if mimes.contains(&key.trim()) {
          continue;
        }
      }
    }
    out.push(line.to_string());
  }

  if in_section && !written {
    out.extend(entries.iter().cloned());
    written = true;
  }
  if !written {
    if !out.is_empty() {
      out.push(String::new());
    }
    out.push(MIMEAPPS_SECTION.to_string());
    out.extend(entries);
  }

  let mut text = out.join("\n");
  if !text.ends_with('\n') {
    text.push('\n');
  }
  text
}

/// Takes `desktop_file` back out of every section of a `mimeapps.list`, leaving
/// the associations that point at other applications alone. Used when
/// uninstalling: a stale default pointing at an application that is gone leaves
/// the desktop with no mail handler at all.
pub fn strip_mimeapps_entries(existing: &str, desktop_file: &str) -> String {
  let mut out: Vec<String> = Vec::new();

  for line in existing.lines() {
    let trimmed = line.trim();
    if trimmed.starts_with('[') {
      out.push(line.to_string());
      continue;
    }
    let Some((key, value)) = trimmed.split_once('=') else {
      out.push(line.to_string());
      continue;
    };

    // A value is a `;`-separated handler list; only our entry goes.
    let kept: Vec<&str> = value
      .split(';')
      .map(str::trim)
      .filter(|entry| !entry.is_empty() && *entry != desktop_file)
      .collect();

    if kept.is_empty() {
      // Nothing but us: drop the association rather than leave it empty.
      continue;
    }
    if kept.len() == value.split(';').filter(|e| !e.trim().is_empty()).count() {
      out.push(line.to_string());
      continue;
    }
    out.push(format!("{}={}", key.trim(), kept.join(";")));
  }

  let mut text = out.join("\n");
  if !text.is_empty() && !text.ends_with('\n') {
    text.push('\n');
  }
  text
}

// ---------------------------------------------------------------------------
// Windows: command lines
// ---------------------------------------------------------------------------

/// `"C:\path\app.exe" --file-attachment "%1"` — the shell command the Explorer
/// context-menu entry runs. `%1` is Explorer's placeholder for the clicked file.
pub fn windows_attach_command(exe: &str) -> String {
  format!("\"{exe}\" --file-attachment \"%1\"")
}

/// `"C:\path\app.exe" "%1"` — used for both the `mailto:` ProgID and the `.eml`
/// ProgID, where `%1` is the URL or the file path.
pub fn windows_open_command(exe: &str) -> String {
  format!("\"{exe}\" \"%1\"")
}

/// Splits an `UninstallString` registry value into program and arguments.
/// Handles both the quoted (`"C:\…\uninstall.exe" /S`) and bare forms.
pub fn split_uninstall_command(raw: &str) -> Option<(String, Vec<String>)> {
  let raw = raw.trim();
  if raw.is_empty() {
    return None;
  }
  if let Some(rest) = raw.strip_prefix('"') {
    let end = rest.find('"')?;
    let program = rest[..end].to_string();
    let args = rest[end + 1..]
      .split_whitespace()
      .map(|s| s.to_string())
      .collect();
    if program.is_empty() {
      return None;
    }
    return Some((program, args));
  }
  // Unquoted: the program is everything up to the first space. Paths with
  // spaces are always quoted by installers, so this is safe.
  let mut parts = raw.split_whitespace();
  let program = parts.next()?.to_string();
  Some((program, parts.map(|s| s.to_string()).collect()))
}

// ---------------------------------------------------------------------------
// macOS: the "Send with guvercin" Quick Action (Services menu) bundle
// ---------------------------------------------------------------------------

/// Shell body of the Quick Action. Input arrives "as arguments", so the
/// selected paths are in `"$@"`. perl (always present on macOS) percent-encodes
/// byte by byte, so multibyte UTF-8 in file names — the U+202F macOS puts in
/// screenshot names, for one — survives `decodeURIComponent`. The whole
/// selection goes over in a single URL, so it becomes a single message.
pub const MACOS_SERVICE_SHELL: &str = concat!(
  "query=''\n",
  "for f in \"$@\"; do\n",
  "    encoded=$(printf '%s' \"$f\" | perl -pe 's/([^A-Za-z0-9._~-])/sprintf(\"%%%02X\", ord($1))/ge')\n",
  "    if [ -z \"$query\" ]; then query=\"path=$encoded\"; else query=\"$query&path=$encoded\"; fi\n",
  "done\n",
  "[ -n \"$query\" ] && open \"guvercin://attach-file?$query\"\n"
);

/// `Contents/Info.plist` of the Quick Action: this is what puts the item in
/// Finder's Services / Quick Actions menu.
pub fn macos_service_info_plist() -> String {
  format!(
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>NSServices</key>
	<array>
		<dict>
			<key>NSMenuItem</key>
			<dict>
				<key>default</key>
				<string>{label}</string>
			</dict>
			<key>NSMessage</key>
			<string>runWorkflowAsService</string>
			<key>NSRequiredContext</key>
			<dict>
				<key>NSApplicationIdentifier</key>
				<string>com.apple.finder</string>
			</dict>
			<key>NSSendFileTypes</key>
			<array>
				<string>public.item</string>
			</array>
		</dict>
	</array>
</dict>
</plist>
"#,
    label = xml_escape(CONTEXT_MENU_LABEL)
  )
}

/// `Contents/document.wflow` — the Automator document itself. A Quick Action
/// without this file is reported by macOS as "damaged or incomplete"; it is not
/// an app bundle, so it has no `Contents/MacOS` executable.
pub fn macos_service_workflow(shell_command: &str) -> String {
  format!(
    r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>AMApplicationBuild</key>
	<string>521</string>
	<key>AMApplicationVersion</key>
	<string>2.10</string>
	<key>AMDocumentVersion</key>
	<string>2</string>
	<key>actions</key>
	<array>
		<dict>
			<key>action</key>
			<dict>
				<key>AMAccepts</key>
				<dict>
					<key>Container</key>
					<string>List</string>
					<key>Optional</key>
					<true/>
					<key>Types</key>
					<array>
						<string>com.apple.cocoa.string</string>
					</array>
				</dict>
				<key>AMActionVersion</key>
				<string>2.0.3</string>
				<key>AMApplication</key>
				<array>
					<string>Automator</string>
				</array>
				<key>AMParameterProperties</key>
				<dict>
					<key>COMMAND_STRING</key>
					<dict/>
					<key>CheckedForUserDefaultShell</key>
					<dict/>
					<key>inputMethod</key>
					<dict/>
					<key>shell</key>
					<dict/>
					<key>source</key>
					<dict/>
				</dict>
				<key>AMProvides</key>
				<dict>
					<key>Container</key>
					<string>List</string>
					<key>Types</key>
					<array>
						<string>com.apple.cocoa.string</string>
					</array>
				</dict>
				<key>ActionBundlePath</key>
				<string>/System/Library/Automator/Run Shell Script.action</string>
				<key>ActionName</key>
				<string>Run Shell Script</string>
				<key>ActionParameters</key>
				<dict>
					<key>COMMAND_STRING</key>
					<string>{command}</string>
					<key>CheckedForUserDefaultShell</key>
					<true/>
					<key>inputMethod</key>
					<integer>1</integer>
					<key>shell</key>
					<string>/bin/bash</string>
					<key>source</key>
					<string></string>
				</dict>
				<key>BundleIdentifier</key>
				<string>com.apple.RunShellScript</string>
				<key>CFBundleVersion</key>
				<string>2.0.3</string>
				<key>CanShowSelectedItemsWhenRun</key>
				<false/>
				<key>CanShowWhenRun</key>
				<true/>
				<key>Category</key>
				<array>
					<string>AMCategoryUtilities</string>
				</array>
				<key>Class Name</key>
				<string>RunShellScriptAction</string>
				<key>InputUUID</key>
				<string>00000000-0000-0000-0000-000000000001</string>
				<key>Keywords</key>
				<array>
					<string>Shell</string>
					<string>Script</string>
					<string>Command</string>
					<string>Run</string>
					<string>Unix</string>
				</array>
				<key>OutputUUID</key>
				<string>00000000-0000-0000-0000-000000000002</string>
				<key>UUID</key>
				<string>00000000-0000-0000-0000-000000000003</string>
				<key>arguments</key>
				<dict/>
				<key>isViewVisible</key>
				<integer>1</integer>
			</dict>
			<key>isViewVisible</key>
			<integer>1</integer>
		</dict>
	</array>
	<key>connectors</key>
	<dict/>
	<key>workflowMetaData</key>
	<dict>
		<key>serviceInputTypeIdentifier</key>
		<string>com.apple.Automator.fileSystemObject</string>
		<key>serviceOutputTypeIdentifier</key>
		<string>com.apple.Automator.nothing</string>
		<key>serviceProcessesInput</key>
		<integer>0</integer>
		<key>workflowTypeIdentifier</key>
		<string>com.apple.Automator.servicesMenu</string>
	</dict>
</dict>
</plist>
"#,
    command = xml_escape(shell_command)
  )
}

// ---------------------------------------------------------------------------
// The unread badge bitmap
// ---------------------------------------------------------------------------

const GLYPH_W: usize = 3;
const GLYPH_H: usize = 5;
const BADGE_BG: [u8; 3] = [0xD9, 0x33, 0x3F];
const BADGE_FG: [u8; 3] = [0xFF, 0xFF, 0xFF];

/// 3×5 bitmap font, one byte per row, the low three bits being the pixels from
/// left to right. Only what a count badge needs: digits and a plus sign.
fn glyph(ch: char) -> Option<[u8; GLYPH_H]> {
  let rows = match ch {
    '0' => [0b111, 0b101, 0b101, 0b101, 0b111],
    '1' => [0b010, 0b110, 0b010, 0b010, 0b111],
    '2' => [0b111, 0b001, 0b111, 0b100, 0b111],
    '3' => [0b111, 0b001, 0b111, 0b001, 0b111],
    '4' => [0b101, 0b101, 0b111, 0b001, 0b001],
    '5' => [0b111, 0b100, 0b111, 0b001, 0b111],
    '6' => [0b111, 0b100, 0b111, 0b101, 0b111],
    '7' => [0b111, 0b001, 0b001, 0b010, 0b010],
    '8' => [0b111, 0b101, 0b111, 0b101, 0b111],
    '9' => [0b111, 0b101, 0b111, 0b001, 0b111],
    '+' => [0b000, 0b010, 0b111, 0b010, 0b000],
    _ => return None,
  };
  Some(rows)
}

/// What the badge reads: the count up to 99, then "99+".
pub fn badge_text(count: u32) -> String {
  match count {
    0 => String::new(),
    1..=99 => count.to_string(),
    _ => "99+".to_string(),
  }
}

/// Renders the unread badge as `size`×`size` RGBA8 — a red disc with the count
/// on it — for platforms that want an image rather than a number (Windows draws
/// it as the taskbar overlay icon). `None` when there is nothing to show.
pub fn badge_rgba(count: u32, size: u32) -> Option<Vec<u8>> {
  if count == 0 || size < 8 {
    return None;
  }
  let text = badge_text(count);
  let w = size as usize;
  let h = size as usize;
  let mut buf = vec![0u8; w * h * 4];

  // Disc, 3×3 supersampled so the edge is not a staircase.
  let centre = size as f32 / 2.0;
  let radius = centre - 0.5;
  for y in 0..h {
    for x in 0..w {
      let mut hits = 0u32;
      for sy in 0..3 {
        for sx in 0..3 {
          let px = x as f32 + (sx as f32 + 0.5) / 3.0;
          let py = y as f32 + (sy as f32 + 0.5) / 3.0;
          let dx = px - centre;
          let dy = py - centre;
          if dx * dx + dy * dy <= radius * radius {
            hits += 1;
          }
        }
      }
      if hits > 0 {
        let i = (y * w + x) * 4;
        buf[i] = BADGE_BG[0];
        buf[i + 1] = BADGE_BG[1];
        buf[i + 2] = BADGE_BG[2];
        buf[i + 3] = ((hits as f32 / 9.0) * 255.0).round() as u8;
      }
    }
  }

  // Text, centred, scaled to the largest whole factor that still fits.
  let chars: Vec<char> = text.chars().collect();
  if !chars.is_empty() {
    let units_w = chars.len() * GLYPH_W + chars.len().saturating_sub(1);
    let inner_w = (size as f32 * 0.78) as usize;
    let inner_h = (size as f32 * 0.72) as usize;
    let scale = (inner_w / units_w).min(inner_h / GLYPH_H).max(1);
    let text_w = units_w * scale;
    let text_h = GLYPH_H * scale;
    let origin_x = w.saturating_sub(text_w) / 2;
    let origin_y = h.saturating_sub(text_h) / 2;

    for (index, ch) in chars.iter().enumerate() {
      let Some(rows) = glyph(*ch) else { continue };
      let char_x = origin_x + index * (GLYPH_W + 1) * scale;
      for (row_index, row) in rows.iter().enumerate() {
        for col in 0..GLYPH_W {
          if row & (1 << (GLYPH_W - 1 - col)) == 0 {
            continue;
          }
          for dy in 0..scale {
            for dx in 0..scale {
              let px = char_x + col * scale + dx;
              let py = origin_y + row_index * scale + dy;
              if px >= w || py >= h {
                continue;
              }
              let i = (py * w + px) * 4;
              buf[i] = BADGE_FG[0];
              buf[i + 1] = BADGE_FG[1];
              buf[i + 2] = BADGE_FG[2];
              buf[i + 3] = 0xFF;
            }
          }
        }
      }
    }
  }

  Some(buf)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn xml_escape_covers_plist_metacharacters() {
    assert_eq!(xml_escape("a & b < c > \"d\" 'e'"), "a &amp; b &lt; c &gt; &quot;d&quot; &apos;e&apos;");
  }

  #[test]
  fn desktop_entry_declares_the_mail_associations() {
    let entry = linux_desktop_entry("/usr/bin/guvercin", "guvercin", "guvercin");
    assert!(entry.contains("Exec=\"/usr/bin/guvercin\" %U"));
    assert!(entry.contains("x-scheme-handler/mailto"));
    assert!(entry.contains("x-scheme-handler/guvercin"));
    assert!(entry.contains("message/rfc822"));

    // The AppImage case: the window's WM_CLASS is the binary inside, not the
    // .AppImage the entry launches, so the two are stated separately.
    let entry = linux_desktop_entry("/home/u/guvercin_0.1.0.AppImage", "guvercin", "guvercin");
    assert!(entry.contains("Exec=\"/home/u/guvercin_0.1.0.AppImage\" %U"));
    assert!(entry.contains("StartupWMClass=guvercin"));
  }

  #[test]
  fn file_manager_entries_carry_the_label_and_binary() {
    assert!(linux_kde_service_menu("/opt/guvercin/guvercin").contains(CONTEXT_MENU_LABEL));
    assert!(linux_kde_service_menu("/opt/guvercin/guvercin").contains("--file-attachment"));
    assert!(linux_nemo_action("/opt/guvercin/guvercin").contains("--file-attachment"));

    let nautilus = linux_nautilus_script();
    assert!(nautilus.contains(ATTACH_URI_BASE));
    // One `path=` parameter per selected file, built before the URL is opened.
    assert!(nautilus.contains("query=\"$query&path=$encoded\""));
    assert!(nautilus.contains("done <<GUVERCIN_EOF"));
  }

  #[test]
  fn handler_match_accepts_any_of_our_desktop_files() {
    let ours = vec!["guvercin.desktop".to_string(), "app-handler.desktop".to_string()];
    assert!(desktop_handler_matches("guvercin.desktop\n", &ours));
    assert!(desktop_handler_matches("app-handler.desktop", &ours));
    assert!(!desktop_handler_matches("thunderbird.desktop", &ours));
    assert!(!desktop_handler_matches("   ", &ours));
  }

  #[test]
  fn mimeapps_default_is_read_back() {
    let text = "[Added Associations]\nmessage/rfc822=other.desktop;\n\n\
                [Default Applications]\nx-scheme-handler/mailto=guvercin.desktop;evolution.desktop\n";
    assert_eq!(
      mimeapps_default_for(text, "x-scheme-handler/mailto").as_deref(),
      Some("guvercin.desktop")
    );
    assert_eq!(mimeapps_default_for(text, "message/rfc822"), None);
  }

  #[test]
  fn mimeapps_upsert_replaces_only_our_keys() {
    let existing = "[Added Associations]\ntext/plain=gedit.desktop\n\n\
                    [Default Applications]\nx-scheme-handler/mailto=thunderbird.desktop\ntext/html=firefox.desktop\n";
    let updated = upsert_mimeapps_defaults(
      existing,
      "guvercin.desktop",
      &["x-scheme-handler/mailto", "message/rfc822"],
    );

    assert!(updated.contains("[Added Associations]"));
    assert!(updated.contains("text/plain=gedit.desktop"));
    assert!(updated.contains("text/html=firefox.desktop"));
    assert!(updated.contains("x-scheme-handler/mailto=guvercin.desktop"));
    assert!(updated.contains("message/rfc822=guvercin.desktop"));
    assert!(!updated.contains("thunderbird"));
    assert_eq!(updated.matches("x-scheme-handler/mailto=").count(), 1);
  }

  #[test]
  fn mimeapps_upsert_creates_the_section_when_missing() {
    let updated = upsert_mimeapps_defaults("", "guvercin.desktop", &["x-scheme-handler/mailto"]);
    assert!(updated.starts_with("[Default Applications]"));
    assert!(updated.ends_with("x-scheme-handler/mailto=guvercin.desktop\n"));

    let updated = upsert_mimeapps_defaults(
      "[Added Associations]\ntext/plain=gedit.desktop\n",
      "guvercin.desktop",
      &["message/rfc822"],
    );
    assert!(updated.contains("[Added Associations]"));
    assert!(updated.contains("[Default Applications]\nmessage/rfc822=guvercin.desktop"));
    assert_eq!(
      mimeapps_default_for(&updated, "message/rfc822").as_deref(),
      Some("guvercin.desktop")
    );
  }

  #[test]
  fn thunar_action_is_merged_next_to_the_user_own_actions() {
    let existing = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n\
                    <action>\n  <name>Open Terminal</name>\n  <unique-id>1234</unique-id>\n</action>\n\
                    </actions>\n";
    let updated = upsert_thunar_action(existing, &linux_thunar_action("/opt/guvercin/guvercin"));

    assert!(updated.contains("Open Terminal"), "the user's own action must survive");
    assert!(updated.contains(CONTEXT_MENU_LABEL));
    assert!(updated.contains("--file-attachment %F"));
    assert!(updated.trim_end().ends_with("</actions>"));
    assert_eq!(updated.matches("<unique-id>guvercin-attach</unique-id>").count(), 1);

    // Installing twice must not stack up two entries.
    let twice = upsert_thunar_action(&updated, &linux_thunar_action("/opt/guvercin/guvercin"));
    assert_eq!(twice.matches("<unique-id>guvercin-attach</unique-id>").count(), 1);
    assert!(twice.contains("Open Terminal"));

    // And removing ours leaves theirs behind.
    let removed = remove_thunar_action(&twice, THUNAR_ACTION_ID);
    assert!(!removed.contains("guvercin-attach"));
    assert!(removed.contains("Open Terminal"));
    assert!(removed.trim_end().ends_with("</actions>"));
  }

  #[test]
  fn thunar_action_creates_a_valid_file_from_nothing() {
    let created = upsert_thunar_action("", &linux_thunar_action("/usr/bin/guvercin"));
    assert!(created.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>"));
    assert!(created.contains("<actions>"));
    assert!(created.trim_end().ends_with("</actions>"));
    // The path is XML-escaped, so a name with a metacharacter cannot break out.
    let odd = upsert_thunar_action("", &linux_thunar_action("/home/a&b/guvercin"));
    assert!(odd.contains("/home/a&amp;b/guvercin"));
    assert!(!odd.contains("/home/a&b/guvercin"));
  }

  #[test]
  fn launcher_badge_signal_carries_the_count() {
    assert_eq!(
      unity_launcher_properties(7),
      "{'count': <int64 7>, 'count-visible': <true>}"
    );
    assert_eq!(
      unity_launcher_properties(0),
      "{'count': <int64 0>, 'count-visible': <false>}"
    );

    // The path must be stable and a legal D-Bus object path.
    let path = unity_launcher_path("guvercin.desktop");
    assert_eq!(path, unity_launcher_path("guvercin.desktop"));
    assert!(path.starts_with("/com/canonical/unity/launcherentry/"));
    assert!(path
      .strip_prefix("/com/canonical/unity/launcherentry/")
      .unwrap()
      .chars()
      .all(|c| c.is_ascii_digit()));
  }

  #[test]
  fn mimeapps_strip_removes_only_our_handler() {
    let existing = "[Default Applications]\n\
                    x-scheme-handler/mailto=guvercin.desktop\n\
                    message/rfc822=guvercin.desktop;evolution.desktop\n\
                    text/html=firefox.desktop\n";
    let stripped = strip_mimeapps_entries(existing, "guvercin.desktop");

    assert!(!stripped.contains("x-scheme-handler/mailto"), "an empty default is dropped");
    assert!(stripped.contains("message/rfc822=evolution.desktop"));
    assert!(stripped.contains("text/html=firefox.desktop"));
    assert!(stripped.contains("[Default Applications]"));
    assert!(!stripped.contains("guvercin.desktop"));
  }

  #[test]
  fn windows_commands_quote_the_executable() {
    let exe = r"C:\Program Files\guvercin\guvercin.exe";
    assert_eq!(
      windows_attach_command(exe),
      r#""C:\Program Files\guvercin\guvercin.exe" --file-attachment "%1""#
    );
    assert_eq!(
      windows_open_command(exe),
      r#""C:\Program Files\guvercin\guvercin.exe" "%1""#
    );
  }

  #[test]
  fn uninstall_string_splits_both_forms() {
    let (program, args) = split_uninstall_command(r#""C:\Program Files\guvercin\uninstall.exe" /S"#).unwrap();
    assert_eq!(program, r"C:\Program Files\guvercin\uninstall.exe");
    assert_eq!(args, vec!["/S".to_string()]);

    let (program, args) = split_uninstall_command(r"C:\app\uninst.exe").unwrap();
    assert_eq!(program, r"C:\app\uninst.exe");
    assert!(args.is_empty());

    assert!(split_uninstall_command("   ").is_none());
  }

  #[test]
  fn quick_action_documents_are_well_formed_enough() {
    let plist = macos_service_info_plist();
    assert!(plist.contains("NSServices"));
    assert!(plist.contains(CONTEXT_MENU_LABEL));

    let workflow = macos_service_workflow(MACOS_SERVICE_SHELL);
    assert!(workflow.contains("com.apple.Automator.servicesMenu"));
    assert!(workflow.contains(ATTACH_URI_BASE));
    // One `path=` parameter per selected file, built before the URL is opened.
    assert!(MACOS_SERVICE_SHELL.contains("query=\"$query&path=$encoded\""));
    // The shell body must arrive XML-escaped: a raw quote would close the
    // <string> element and leave Automator with a damaged document.
    assert!(workflow.contains("&quot;$f&quot;"));
    assert!(!workflow.contains(r#""$f""#));
  }

  #[test]
  fn badge_text_caps_at_ninety_nine() {
    assert_eq!(badge_text(0), "");
    assert_eq!(badge_text(1), "1");
    assert_eq!(badge_text(99), "99");
    assert_eq!(badge_text(100), "99+");
  }

  #[test]
  fn badge_bitmap_has_the_right_shape() {
    assert!(badge_rgba(0, 32).is_none());
    let pixels = badge_rgba(7, 32).expect("a badge for 7");
    assert_eq!(pixels.len(), 32 * 32 * 4);

    // Corner is outside the disc, centre is inside it.
    assert_eq!(pixels[3], 0, "top-left corner must be transparent");
    let centre = ((16 * 32) + 16) * 4;
    assert_eq!(pixels[centre + 3], 0xFF, "centre must be opaque");

    // The digit is drawn in white on the red disc.
    let white = pixels
      .chunks_exact(4)
      .any(|p| p == [0xFF, 0xFF, 0xFF, 0xFF]);
    assert!(white, "the count must be drawn on the badge");
  }

  #[test]
  fn badge_bitmap_fits_three_characters() {
    let pixels = badge_rgba(1234, 32).expect("a badge for 1234");
    assert_eq!(pixels.len(), 32 * 32 * 4);
    let white = pixels.chunks_exact(4).filter(|p| p[3] == 0xFF && p[0] == 0xFF).count();
    assert!(white > 0, "\"99+\" must be drawn");
  }
}

