/**
 * Options every mail-rendering surface passes to sanitizeMailHtml().
 *
 * There are four of them (reading pane, tab view, thread reader, detached
 * window) and they must agree on the remote-image policy, so the lookup lives
 * here rather than being re-derived at each call site.
 */

import { getUIPreferences } from './uiPreferences.js'

/**
 * @param {string|number|null|undefined} accountId
 * @param {(key: string, fallback?: string) => string} [t] i18next translator
 */
export function getMailSanitizeOptions(accountId, t) {
  const translate = typeof t === 'function' ? t : (key) => key
  return {
    remoteImages: getUIPreferences(accountId).remoteImageMode,
    labels: {
      // Keys are the English strings, matching the rest of the catalogue.
      // Keep them free of '.' — i18next's default keySeparator would split on it.
      blocked: translate('Remote images in this message were not loaded'),
      load: translate('Load images'),
    },
  }
}
