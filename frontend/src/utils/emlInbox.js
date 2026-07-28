// Bridges OS-level file associations (`.eml` / `.msg`) to the mail viewer.
//
// When the user double-clicks a message file, the OS launches (or re-focuses)
// guvercin and hands it the file. The three platforms deliver it differently:
// macOS sends a `file://` deep link, while Windows and Linux pass a plain path
// as a launch argument — which is not a URL, so the deep-link plugin ignores it.
// The Rust side parses those arguments and offers them through
// `take_launch_files` (cold start) and the `os://open-file` event (already
// running), so all three arrive here the same way.
//
// Like mailto links these can turn up before an account is active, so paths are
// buffered in a queue and drained by whichever consumer (typically
// DashboardPage) is ready. Listeners install once.

const queue = []
const subscribers = new Set()
let initialized = false

function isTauri() {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

// Decodes a `file://` URL (or bare path) into a filesystem path and keeps only
// message files we know how to import. Returns null for anything else so the
// shared deep-link stream (which also carries mailto: links) is ignored safely.
// Exported for the tests: it has to hold for all three platforms' shapes.
export function toEmlPath(url) {
  if (typeof url !== 'string') return null
  let path = url.trim()
  if (!path) return null

  if (/^file:\/\//i.test(path)) {
    path = path.replace(/^file:\/\/(localhost)?/i, '')
    try {
      path = decodeURIComponent(path)
    } catch {
      // Leave the raw path if it isn't valid percent-encoding.
    }
  } else if (/^[a-z]:[\\/]/i.test(path)) {
    // A Windows path (C:\Users\…\note.eml). It looks like a URL scheme to the
    // test below, so it has to be recognised first.
  } else if (/^[a-z][a-z0-9+.-]*:/i.test(path)) {
    // Some other scheme (mailto:, tel:, …) — not a file.
    return null
  }

  const lower = path.toLowerCase()
  if (!lower.endsWith('.eml') && !lower.endsWith('.msg')) return null
  return path
}

function dispatch(path) {
  if (subscribers.size === 0) {
    queue.push(path)
    return
  }
  for (const cb of subscribers) {
    try {
      cb(path)
    } catch (error) {
      console.error('eml subscriber failed:', error)
    }
  }
}

function handleUrls(urls) {
  if (!Array.isArray(urls)) return
  for (const url of urls) {
    const path = toEmlPath(url)
    if (path) dispatch(path)
  }
}

// Installs the deep-link and launch-argument listeners. Safe to call multiple
// times.
export async function initEmlInbox() {
  if (initialized || !isTauri()) return
  initialized = true

  // Files passed as launch arguments (Windows/Linux), collected by the Rust
  // side before any window existed.
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    const pending = await invoke('take_launch_files')
    handleUrls(pending)
  } catch (error) {
    console.error('Failed to read the launch files:', error)
  }

  // Files passed as arguments to a second launch, forwarded to this instance.
  try {
    const { listen } = await import('@tauri-apps/api/event')
    await listen('os://open-file', (event) => handleUrls(event?.payload))
  } catch (error) {
    console.error('Failed to listen for opened files:', error)
  }

  try {
    const { onOpenUrl, getCurrent } = await import('@tauri-apps/plugin-deep-link')
    // Files the app was launched with (cold start).
    try {
      const current = await getCurrent()
      handleUrls(current)
    } catch {
      // getCurrent is unavailable on some platforms; ignore.
    }
    // Files delivered while the app is running (hot).
    await onOpenUrl(handleUrls)
  } catch (error) {
    console.error('Failed to initialize eml file-association handling:', error)
  }
}

// Subscribe to incoming file paths. Immediately flushes any paths that arrived
// before a subscriber was present. Returns an unsubscribe fn.
export function subscribeEml(callback) {
  if (typeof callback !== 'function') return () => {}
  subscribers.add(callback)
  if (queue.length > 0) {
    const pending = queue.splice(0, queue.length)
    for (const path of pending) {
      try {
        callback(path)
      } catch (error) {
        console.error('eml subscriber failed:', error)
      }
    }
  }
  return () => {
    subscribers.delete(callback)
  }
}
