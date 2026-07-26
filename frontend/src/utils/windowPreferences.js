/**
 * Manages window size and position preferences.
 * Stores in localStorage so preferences persist across app restarts.
 */

const WINDOW_PREFS_KEY = 'window_preferences'
const WINDOW_FIRST_LAUNCH_KEY = 'window_first_launch'

function getStorageKey() {
  return WINDOW_PREFS_KEY
}

function isFirstLaunch() {
  try {
    const val = localStorage.getItem(WINDOW_FIRST_LAUNCH_KEY)
    return val !== 'false'
  } catch {
    return true
  }
}

function markFirstLaunchDone() {
  try {
    localStorage.setItem(WINDOW_FIRST_LAUNCH_KEY, 'false')
  } catch {
    // storage unavailable, ignore
  }
}

export function getSavedWindowPreferences() {
  try {
    const raw = localStorage.getItem(getStorageKey())
    if (!raw) return null
    const parsed = JSON.parse(raw)
    // Validate the shape
    if (
      parsed &&
      typeof parsed === 'object' &&
      typeof parsed.width === 'number' &&
      typeof parsed.height === 'number' &&
      typeof parsed.x === 'number' &&
      typeof parsed.y === 'number'
    ) {
      return parsed
    }
  } catch {
    // parse error or storage unavailable
  }
  return null
}

export function saveWindowPreferences(width, height, x, y) {
  try {
    const prefs = {
      width: Math.round(width),
      height: Math.round(height),
      x: Math.round(x),
      y: Math.round(y),
      timestamp: Date.now(),
    }
    localStorage.setItem(getStorageKey(), JSON.stringify(prefs))
  } catch {
    // storage unavailable, ignore
  }
}

export function shouldUseFullscreen() {
  return isFirstLaunch()
}

export function markAuthenticationComplete() {
  markFirstLaunchDone()
}
