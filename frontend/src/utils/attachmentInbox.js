// Bridges the file manager's "Send with guvercin" entry to the compose UI.
//
// The entry reaches us two ways, depending on what the platform's file manager
// can launch: a `guvercin://attach-file?path=<path>` URI (the macOS Quick
// Action and the Nautilus script) or a `--file-attachment <path>` argument
// (Explorer, Dolphin, Nemo). Arguments given at cold start are collected by the
// Rust side and handed over through `take_launch_attachments`; while the app is
// already running the Rust side opens the compose window itself. Both end in
// the same place.

import { invoke } from '@tauri-apps/api/core'

let initialized = false

function isTauri() {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

// Reads the paths out of `guvercin://attach-file?path=<a>&path=<b>…`. A file
// manager hands over the whole selection in one URI, so this returns a list.
// Exported for the tests.
export function parseAttachmentUri(uri) {
  if (typeof uri !== 'string') return []

  const match = uri.match(/^guvercin:\/\/attach-file\?(.+)$/)
  if (!match) return []

  const paths = []
  for (const part of match[1].split('&')) {
    if (!part.startsWith('path=')) continue
    const encoded = part.slice('path='.length)
    if (!encoded) continue
    try {
      paths.push(decodeURIComponent(encoded))
    } catch {
      console.error('Failed to decode an attachment path:', encoded)
    }
  }
  return paths
}

function handleUrls(urls) {
  if (!Array.isArray(urls)) return
  const paths = []
  for (const url of urls) {
    if (typeof url === 'string' && url.startsWith('guvercin://attach-file')) {
      paths.push(...parseAttachmentUri(url))
    }
  }
  if (paths.length > 0) void attachFiles(paths)
}

// A whole selection becomes one message with every file attached, so it goes
// over in a single call rather than one per file.
async function attachFiles(paths) {
  const filePaths = (Array.isArray(paths) ? paths : [])
    .filter((path) => typeof path === 'string' && path.trim())
  if (filePaths.length === 0) return

  try {
    await invoke('attach_files_to_compose', { filePaths })
  } catch (error) {
    console.error('Failed to attach files:', error)
  }
}

// Installs the deep-link and launch-argument listeners. Safe to call multiple
// times.
export async function initAttachmentInbox() {
  if (initialized || !isTauri()) return
  initialized = true

  // Files handed over as launch arguments before any window existed. The Rust
  // side already opened a compose window for a second launch, so this only
  // covers the cold start.
  try {
    const pending = await invoke('take_launch_attachments')
    await attachFiles(pending)
  } catch (error) {
    console.error('Failed to read the launch attachments:', error)
  }

  try {
    const { onOpenUrl, getCurrent } = await import('@tauri-apps/plugin-deep-link')
    // URIs the app was launched with (cold start).
    try {
      const current = await getCurrent()
      handleUrls(current)
    } catch {
      // getCurrent is unavailable on some platforms; ignore.
    }
    // URIs delivered while the app is running (hot).
    await onOpenUrl(handleUrls)
  } catch (error) {
    console.error('Failed to initialize attachment deep-link handling:', error)
  }
}
