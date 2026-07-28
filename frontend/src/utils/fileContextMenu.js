// The file manager's "Send with guvercin" entry.
//
// guvercin installs it once on first launch on every platform — a Finder Quick
// Action on macOS, a context-menu verb in Explorer, drop-in files for Nautilus,
// Dolphin and Nemo. These helpers let the user put it back if they removed it,
// or take it away if they don't want it.

function isTauri() {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

export async function isFileContextMenuRegistered() {
  if (!isTauri()) return false
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    return Boolean(await invoke('is_file_context_menu_registered'))
  } catch (error) {
    console.error('is_file_context_menu_registered failed:', error)
    return false
  }
}

// Returns { ok, message } — `message` carries the reason when it didn't work.
export async function setFileContextMenuRegistered(enabled) {
  if (!isTauri()) return { ok: false, message: '' }
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    await invoke(enabled ? 'register_file_context_menu' : 'unregister_file_context_menu')
    return { ok: true, message: '' }
  } catch (error) {
    console.error('file context menu change failed:', error)
    return { ok: false, message: error?.message || String(error) }
  }
}
