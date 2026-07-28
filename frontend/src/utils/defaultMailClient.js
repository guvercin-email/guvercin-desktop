// Helpers for making guvercin the OS default `mailto:` handler.
//
// Every desktop platform is supported, through whichever mechanism it offers:
// LaunchServices on macOS, `xdg-mime`/`xdg-settings` on Linux, and on Windows a
// registration in `Software\Clients\Mail` followed by the Default apps page —
// Windows deliberately reserves the final choice for the user. That last case
// is why setting the default reports back an outcome instead of a bare boolean:
// the caller has to be able to say "now pick guvercin in the page that opened"
// rather than claim a success that has not happened yet.

const PROMPT_FLAG = 'default_mail_prompt_shown'

function isTauri() {
  return typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window
}

export async function isDefaultMailClient() {
  if (!isTauri()) return false
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    return Boolean(await invoke('is_default_mail_client'))
  } catch (error) {
    console.error('is_default_mail_client failed:', error)
    return false
  }
}

// Asks the OS to hand guvercin the `mailto:` association. Returns
// { ok, isDefault, needsUserAction, message }:
//   ok               — the request itself went through
//   isDefault        — guvercin holds the association now
//   needsUserAction  — the OS is waiting for the user to confirm it
//   message          — what to show the user when it isn't simply done
export async function setAsDefaultMailClient() {
  if (!isTauri()) {
    return { ok: false, isDefault: false, needsUserAction: false, message: '' }
  }
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    const outcome = await invoke('set_as_default_mail_client')
    return {
      ok: true,
      isDefault: Boolean(outcome?.isDefault),
      needsUserAction: Boolean(outcome?.needsUserAction),
      message: outcome?.message || '',
    }
  } catch (error) {
    console.error('set_as_default_mail_client failed:', error)
    return {
      ok: false,
      isDefault: false,
      needsUserAction: false,
      message: error?.message || String(error),
    }
  }
}

// Whether the first-launch prompt has already been shown (and answered) once.
export function hasShownDefaultPrompt() {
  try {
    return localStorage.getItem(PROMPT_FLAG) === '1'
  } catch {
    return false
  }
}

export function markDefaultPromptShown() {
  try {
    localStorage.setItem(PROMPT_FLAG, '1')
  } catch {
    // ignore storage failures
  }
}
