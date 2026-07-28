const WEB_FALLBACK_KEY = 'guv_link_click_behavior'

function isProbablyTauri() {
  return typeof window !== 'undefined' && !!(window.__TAURI_INTERNALS__ || window.__TAURI__)
}

function normalizeExternalUrl(input) {
  let u = String(input || '')
  // Some HTML/JS content includes zero-width chars that break URL parsing.
  u = u.replace(/[\u200B-\u200D\uFEFF]/g, '')
  u = u.trim()
  if (!u) return null

  // Protocol-relative URLs (common in HTML emails)
  if (u.startsWith('//')) return `https:${u}`

  if (/^(https?:\/\/|mailto:|tel:)/i.test(u)) {
    return u
  }

  // Common emails/buttons use scheme-less URLs like "www.example.com" or "example.com/path".
  // Normalize to https:// so both open/copy and URL parsing work consistently.
  if (u.toLowerCase().startsWith('www.')) return `https://${u}`

  // Very small heuristic: treat plain domains as https://<domain>
  // (avoid spaces, angle brackets, quotes, and obvious non-domains).
  if (
    /^[a-z0-9.-]+\.[a-z]{2,}(?::\d{2,5})?(?:[/?#][^\s<>"']*)?$/i.test(u)
    && !u.includes('@')
  ) {
    return `https://${u}`
  }

  return null
}

async function tryInvoke(command, args) {
  try {
    const { invoke } = await import('@tauri-apps/api/core')
    return await invoke(command, args)
  } catch {
    return null
  }
}

export function isAllowedExternalUrl(url) {
  return !!normalizeExternalUrl(url)
}

export function getUrlDomain(url) {
  try {
    const normalized = normalizeExternalUrl(url)
    if (!normalized) return null
    const u = new URL(normalized)
    return u.hostname.toLowerCase()
  } catch {
    return null
  }
}

/**
 * Sanitize email HTML to neutralize all external links.
 *
 * Every <a> with an external href gets its href moved to
 * `data-external-href` and replaced with "#", so the browser never
 * attempts to navigate the sandboxed iframe to an external URL.
 * <base> tags and target attributes on links are also stripped.
 *
 * The link interceptor (installIframeLinkInterceptor) reads from
 * data-external-href to present the open/copy prompt.
 */
// Resolve the app's current text color so mail rendered in the
// sandboxed iframe inherits a readable color for the active theme.
// Guarded for non-browser (unit test) environments.
function readThemeCanvas() {
  const fallback = { textColor: '#1a1a1a' }
  try {
    if (typeof document === 'undefined' || !document.documentElement) return fallback
    const root = document.documentElement
    const themeName = (root.dataset.theme || '').toLowerCase()
    let isDark
    if (themeName.includes('dark')) isDark = true
    else if (themeName.includes('light')) isDark = false
    else isDark = !!(window.matchMedia && window.matchMedia('(prefers-color-scheme: dark)').matches)
    const text = getComputedStyle(root).getPropertyValue('--c-text-1').trim()
    const textColor = text || (isDark ? '#e8e8e8' : '#1a1a1a')
    return { textColor }
  } catch {
    return fallback
  }
}

/** Marks a document whose remote images were withheld. */
export const BLOCKED_REMOTE_ATTR = 'data-blocked-remote'

const REMOTE_BAR_ID = '__guvercin_remote_bar'

const DEFAULT_REMOTE_LABELS = {
  blocked: 'Remote images in this message were not loaded.',
  load: 'Load images',
}

function isRemoteUrl(value) {
  return /^\s*(https?:)?\/\//i.test(String(value || ''))
}

/**
 * Withhold anything that would make the reading pane talk to the sender's
 * server on open — the tracking-pixel problem. Remote `src` values are parked
 * in `data-blocked-src` (so "load images" can put them back without
 * re-fetching the message) and remote `url()` backgrounds are dropped from
 * inline styles. Returns how many elements were held back.
 */
function blockRemoteAssets(doc) {
  let blocked = 0

  doc.querySelectorAll('img[src], input[type="image"][src]').forEach((el) => {
    const src = el.getAttribute('src')
    if (!isRemoteUrl(src)) return
    el.setAttribute('data-blocked-src', src)
    el.removeAttribute('src')
    blocked += 1
  })

  doc.querySelectorAll('[style]').forEach((el) => {
    const style = el.getAttribute('style') || ''
    if (!/url\(/i.test(style)) return
    const stripped = style.replace(/url\(\s*['"]?\s*(?:https?:)?\/\/[^)]*\)/gi, 'none')
    if (stripped === style) return
    el.setAttribute('data-blocked-style', style)
    el.setAttribute('style', stripped)
    blocked += 1
  })

  // Stylesheets the message brings with it can pull remote assets too, and
  // there is no way to rewrite them reliably — drop the external ones.
  doc.querySelectorAll('link[rel~="stylesheet"][href]').forEach((el) => {
    if (!isRemoteUrl(el.getAttribute('href'))) return
    el.remove()
    blocked += 1
  })

  return blocked
}

/**
 * Add the "remote images were not loaded / load images" bar to a document
 * whose assets were withheld.
 *
 * The button is wired by a script we inject ourselves, after every script the
 * message brought with it has already been removed. Doing it this way means
 * the control works in every reading surface (pane, tab, detached window)
 * without each one having to wire up a handler, and restoring an image costs
 * nothing extra — the URL is already parked on the element.
 */
function appendRemoteImageBar(doc, labels) {
  const body = doc.body
  if (!body) return

  const bar = doc.createElement('div')
  bar.id = REMOTE_BAR_ID
  const text = doc.createElement('span')
  text.textContent = labels.blocked
  const button = doc.createElement('button')
  button.type = 'button'
  button.textContent = labels.load
  bar.appendChild(text)
  bar.appendChild(button)
  body.insertBefore(bar, body.firstChild)

  const style = doc.createElement('style')
  style.textContent =
    `#${REMOTE_BAR_ID}{display:flex !important;align-items:center;gap:.75em;` +
    'flex-wrap:wrap;margin:0 0 1em;padding:.6em .8em;border-radius:6px;' +
    'background:rgba(127,127,127,.16) !important;font:inherit;font-size:.9em}' +
    `#${REMOTE_BAR_ID} button{cursor:pointer;padding:.35em .8em;border-radius:5px;` +
    'border:1px solid currentColor;background:transparent !important;' +
    'color:inherit !important;font:inherit;font-size:inherit}'
  doc.head.appendChild(style)

  const script = doc.createElement('script')
  script.textContent =
    `(function(){var b=document.getElementById(${JSON.stringify(REMOTE_BAR_ID)});` +
    'if(!b)return;var t=b.querySelector("button");if(!t)return;' +
    't.addEventListener("click",function(){' +
    'document.querySelectorAll("[data-blocked-src]").forEach(function(el){' +
    'el.setAttribute("src",el.getAttribute("data-blocked-src"));' +
    'el.removeAttribute("data-blocked-src")});' +
    'document.querySelectorAll("[data-blocked-style]").forEach(function(el){' +
    'el.setAttribute("style",el.getAttribute("data-blocked-style"));' +
    'el.removeAttribute("data-blocked-style")});' +
    `document.body.removeAttribute(${JSON.stringify(BLOCKED_REMOTE_ATTR)});` +
    'b.remove();' +
    'window.dispatchEvent(new Event("guvercin-remote-images-loaded"))})})()'
  body.appendChild(script)
}

/**
 * Put back what blockRemoteAssets() withheld, in a live document. Used by the
 * "load images" control so the message does not have to be re-fetched.
 */
export function allowRemoteAssets(doc) {
  if (!doc) return 0
  let restored = 0
  doc.querySelectorAll('[data-blocked-src]').forEach((el) => {
    el.setAttribute('src', el.getAttribute('data-blocked-src'))
    el.removeAttribute('data-blocked-src')
    restored += 1
  })
  doc.querySelectorAll('[data-blocked-style]').forEach((el) => {
    el.setAttribute('style', el.getAttribute('data-blocked-style'))
    el.removeAttribute('data-blocked-style')
    restored += 1
  })
  doc.getElementById?.(REMOTE_BAR_ID)?.remove()
  if (doc.body) doc.body.removeAttribute(BLOCKED_REMOTE_ATTR)
  return restored
}

/** How many remote assets a sanitized document is currently withholding. */
export function countBlockedRemoteAssets(doc) {
  const raw = doc?.body?.getAttribute?.(BLOCKED_REMOTE_ATTR)
  const n = Number(raw)
  return Number.isFinite(n) && n > 0 ? n : 0
}

/**
 * @param {string} html
 * @param {{ remoteImages?: 'auto'|'block'|'prompt',
 *           labels?: { blocked?: string, load?: string } }} [options]
 *   `auto` loads remote assets, `block` withholds them for good, `prompt`
 *   withholds them behind a "load images" bar. Omitting it withholds them: a
 *   caller that forgets to pass the account preference should fail towards
 *   privacy, not away from it. `labels` supplies translated bar text.
 */
export function sanitizeMailHtml(html, options = {}) {
  if (!html) return html
  const remoteImages = ['auto', 'block', 'prompt'].includes(options.remoteImages)
    ? options.remoteImages
    : 'block'
  try {
    const doc = new DOMParser().parseFromString(html, 'text/html')

    const extractUrlFromJs = (js) => {
      let s = String(js || '')
      if (!s) return null
      // Convert common escaped JS string forms to plain URLs.
      s = s.replace(/\\\//g, '/')
      s = s.replace(/\\u002f/gi, '/')
      s = s.replace(/[\u200B-\u200D\uFEFF]/g, '')

      // Try to find a URL-like token in common redirect patterns.
      // Keep it conservative: first matching candidate wins.
      const candidates = []
      // http(s)://... or //example.com/... (protocol-relative)
      const m0 = s.match(/((?:https?:)?\/\/[^\s"'<>]+)/i)
      if (m0?.[1]) candidates.push(m0[1])
      const m2 = s.match(/(mailto:[^\s"'<>]+)/i)
      if (m2?.[1]) candidates.push(m2[1])
      const m3 = s.match(/(tel:[^\s"'<>]+)/i)
      if (m3?.[1]) candidates.push(m3[1])
      const m4 = s.match(/(www\.[^\s"'<>]+)/i)
      if (m4?.[1]) candidates.push(m4[1])
      const m5 = s.match(/(?:^|[="'\s])(?!javascript:)([a-z0-9.-]+\.[a-z]{2,}(?:[^\s"'<>]*)?)/i)
      if (m5?.[1]) candidates.push(m5[1])

      for (const c of candidates) {
        const normalized = normalizeExternalUrl(c)
        if (normalized) return normalized
      }
      return null
    }

    const markElementAsExternalLink = (el, url) => {
      const normalized = normalizeExternalUrl(url)
      if (!normalized) return
      el.setAttribute('data-external-href', normalized)
      if (el.tagName !== 'A') {
        // Make it feel clickable inside the sandboxed iframe.
        if (!el.getAttribute('role')) el.setAttribute('role', 'link')
        if (!el.hasAttribute('tabindex')) el.setAttribute('tabindex', '0')
        try {
          el.style.cursor = 'pointer'
        } catch {
          // ignore
        }
      }
    }

    // ── Remove dangerous elements ──────────────────────────────────
    // Scripts, iframes, objects, embeds, applets, and forms can all
    // execute code or trick the user into submitting data.
    doc.querySelectorAll(
      'script, iframe, object, embed, applet, form, link[rel="import"]'
    ).forEach((el) => el.remove())

    // Remove <base> elements — they can redirect relative link resolution
    doc.querySelectorAll('base').forEach((el) => el.remove())

    // Remove CSP meta tags from the email — they serve no purpose in our
    // sandboxed iframe and can trigger browser warnings (e.g. manifest-src)
    doc.querySelectorAll('meta[http-equiv="Content-Security-Policy"]').forEach((el) => el.remove())
    doc.querySelectorAll('meta[http-equiv="content-security-policy"]').forEach((el) => el.remove())

    // ── Strip inline event handlers & javascript: URLs ─────────────
    // This prevents any script execution from email HTML, even with
    // allow-scripts in the sandbox (needed for parent event listeners).
    doc.querySelectorAll('*').forEach((el) => {
      // Remove all on* attributes (onclick, onload, onerror, etc.)
      const attrsToRemove = []
      for (const attr of el.attributes) {
        if (/^on/i.test(attr.name)) {
          // Preserve simple redirect intent (common in "button" emails) as a safe data-external-href.
          const inferred = extractUrlFromJs(attr.value)
          if (inferred) markElementAsExternalLink(el, inferred)
          attrsToRemove.push(attr.name)
        }
      }
      attrsToRemove.forEach((name) => el.removeAttribute(name))

      // Neutralize javascript: hrefs (but not data-external-href ones)
      const href = el.getAttribute('href')
      if (href && /^\s*javascript\s*:/i.test(href)) {
        const inferred = extractUrlFromJs(href)
        if (inferred) markElementAsExternalLink(el, inferred)
        el.setAttribute('href', '#')
      }
      const src = el.getAttribute('src')
      if (src && /^\s*javascript\s*:/i.test(src)) {
        el.removeAttribute('src')
      }

      // Some templates use formaction on <button>/<input> outside real forms.
      const formaction = el.getAttribute('formaction')
      if (formaction) {
        markElementAsExternalLink(el, formaction)
        el.removeAttribute('formaction')
      }

      // Some email builders store the real link in data-* attributes.
      // If present, preserve them as a safe clickable external link.
      const dataLinkCandidates = [
        el.getAttribute('data-external-href'),
        el.getAttribute('data-saferedirecturl'),
        el.getAttribute('data-mce-href'),
        el.getAttribute('data-cke-saved-href'),
        el.getAttribute('data-href'),
        el.getAttribute('data-url'),
        el.getAttribute('data-link'),
      ]
      for (const candidate of dataLinkCandidates) {
        const normalized = normalizeExternalUrl(candidate)
        if (normalized) {
          markElementAsExternalLink(el, normalized)
          break
        }
      }
    })

    // ── Neutralize external links ──────────────────────────────────
    doc.querySelectorAll('a[href], area[href]').forEach((linkEl) => {
      const candidates = [
        linkEl.getAttribute('data-external-href'),
        linkEl.getAttribute('href'),
        linkEl.getAttribute('data-saferedirecturl'),
        linkEl.getAttribute('data-mce-href'),
        linkEl.getAttribute('data-cke-saved-href'),
        linkEl.getAttribute('data-href'),
        linkEl.getAttribute('data-url'),
        linkEl.getAttribute('data-link'),
      ]
      const normalized = candidates.map((c) => normalizeExternalUrl(c)).find(Boolean) || null
      if (normalized) {
        // Store original URL and neutralize the link
        linkEl.setAttribute('data-external-href', normalized)
        linkEl.setAttribute('href', '#')
        linkEl.removeAttribute('target')
        if (linkEl.tagName === 'A') {
          linkEl.style.cursor = 'pointer'
        }
      }
    })

    // Also remove target attributes from any remaining links
    doc.querySelectorAll('a[target]').forEach((anchor) => {
      anchor.removeAttribute('target')
    })

    // ── Neutralize image loading issues ────────────────────────────
    // Sandboxed iframes often have issues with loading="lazy", causing images
    // to flash and disappear or fail to load depending on viewport intersection.
    doc.querySelectorAll('img').forEach((img) => {
      try {
        // If an <img> only provides a srcset (no src), move the first candidate
        // to src so the image remains visible after we strip srcset.
        const currentSrc = img.getAttribute('src')
        const srcset = img.getAttribute('srcset')
        if ((!currentSrc || String(currentSrc).trim() === '') && srcset) {
          const first = String(srcset).split(',')[0].trim().split(/\s+/)[0]
          if (first && !/^\s*javascript\s*:/i.test(first)) {
            img.setAttribute('src', first)
          }
        }
      } catch {
        // ignore any DOM quirks
      }
      img.removeAttribute('loading')
      // Some emails use srcset which can cause similar issues in WebKit/WebView2
      // if the source resolutions fail or are blocked. We'll strip it to ensure
      // the fallback src is always used reliably.
      img.removeAttribute('srcset')
    })

    // ── Blend mail into the current theme ──────────────────────────
    // Emails render in a sandboxed iframe. Whatever background the message
    // hardcodes (or the UA canvas default) paints a box that stands out
    // from the app — and forcing color-scheme:dark makes the canvas solid
    // black, which is the same problem. Instead make the whole document
    // transparent so the app's real background shows straight through, and
    // force every element to the app's text color so the message stays
    // readable in the active theme. color-scheme:normal keeps the UA from
    // repainting an opaque canvas behind the transparent document.
    const { textColor } = readThemeCanvas()
    let head = doc.head
    if (!head) {
      head = doc.createElement('head')
      doc.documentElement.insertBefore(head, doc.documentElement.firstChild)
    }
    const baseStyle = doc.createElement('style')
    baseStyle.textContent =
      'html,body{color-scheme:normal !important;background:transparent !important}' +
      `html,body,body *{background-color:transparent !important;background-image:none !important;color:${textColor} !important}` +
      'a,a *{color:inherit !important;text-decoration:underline}'
    head.appendChild(baseStyle)

    // ── Withhold remote assets ─────────────────────────────────────
    // Last, so it also catches the src values moved out of srcset above.
    if (remoteImages !== 'auto') {
      const blocked = blockRemoteAssets(doc)
      if (blocked > 0 && doc.body) {
        doc.body.setAttribute(BLOCKED_REMOTE_ATTR, String(blocked))
        // 'block' is "never load"; only 'prompt' offers the way back.
        if (remoteImages === 'prompt') {
          appendRemoteImageBar(doc, { ...DEFAULT_REMOTE_LABELS, ...(options.labels || {}) })
        }
      }
    }

    // Serialize back — use the full document including <html>/<head>/<body>
    // so styles/meta are preserved
    return doc.documentElement.outerHTML
  } catch {
    // If parsing fails, return original HTML as-is (better than nothing)
    return html
  }
}

export async function getLinkClickBehavior(url) {
  const domain = url ? getUrlDomain(url) : null
  
  if (isProbablyTauri()) {
    if (domain) {
      const dv = await tryInvoke('get_domain_link_behavior', { domain })
      if (dv === 'open' || dv === 'copy' || dv === 'ask') return dv
    }
    const v = await tryInvoke('get_link_click_behavior', {})
    if (v === 'ask' || v === 'open' || v === 'copy') return v
    return 'ask'
  }

  // Web fallback (using a prefixed key for domains in localStorage if we want, but let's keep it simple for now)
  if (domain) {
    const dv = window.localStorage?.getItem(`${WEB_FALLBACK_KEY}_domain_${domain}`)
    if (dv === 'open' || dv === 'copy' || dv === 'ask') return dv
  }
  const v = window.localStorage?.getItem(WEB_FALLBACK_KEY)
  return v === 'ask' || v === 'open' || v === 'copy' ? v : 'ask'
}

export async function setLinkClickBehavior(value) {
  const v = value === 'open' || value === 'copy' || value === 'ask' ? value : 'ask'
  if (isProbablyTauri()) {
    await tryInvoke('set_link_click_behavior', { behavior: v })
    return
  }
  window.localStorage?.setItem(WEB_FALLBACK_KEY, v)
}

export async function setDomainLinkBehavior(domain, value) {
  const v = value === 'open' || value === 'copy' || value === 'ask' ? value : 'ask'
  if (!domain) return
  if (isProbablyTauri()) {
    await tryInvoke('set_domain_link_behavior', { domain, behavior: v })
    return
  }
  window.localStorage?.setItem(`${WEB_FALLBACK_KEY}_domain_${domain}`, v)
}

export async function removeDomainLinkBehavior(domain) {
  if (!domain) return
  if (isProbablyTauri()) {
    await tryInvoke('remove_domain_link_behavior', { domain })
    return
  }
  window.localStorage?.removeItem(`${WEB_FALLBACK_KEY}_domain_${domain}`)
}

export async function getAllDomainLinkBehaviors() {
  if (isProbablyTauri()) {
    return (await tryInvoke('get_all_domain_link_behaviors', {})) || {}
  }
  
  // Web fallback
  const results = {}
  const prefix = `${WEB_FALLBACK_KEY}_domain_`
  for (let i = 0; i < localStorage.length; i++) {
    const key = localStorage.key(i)
    if (key.startsWith(prefix)) {
      results[key.slice(prefix.length)] = localStorage.getItem(key)
    }
  }
  return results
}

export async function openExternalUrl(url) {
  const normalized = normalizeExternalUrl(url)
  if (!normalized) return

  if (isProbablyTauri()) {
    await tryInvoke('open_external_url', { url: normalized })
    return
  }

  window.open(normalized, '_blank', 'noopener,noreferrer')
}

export async function copyTextToClipboard(text) {
  const t = String(text ?? '')
  if (isProbablyTauri()) {
    await tryInvoke('copy_to_clipboard', { text: t })
    return
  }

  if (navigator?.clipboard?.writeText) {
    await navigator.clipboard.writeText(t)
    return
  }

  const el = document.createElement('textarea')
  el.value = t
  el.setAttribute('readonly', 'true')
  el.style.position = 'fixed'
  el.style.left = '-9999px'
  document.body.appendChild(el)
  el.select()
  document.execCommand('copy')
  el.remove()
}

/**
 * Install a link-click interceptor on an iframe's document.
 *
 * This works with sanitizeMailHtml(): links have their original
 * href in data-external-href and their actual href set to "#".
 * When the user clicks a link, the callback receives the original URL.
 *
 * Supports both:
 *   - Sanitized HTML (data-external-href) — preferred, always works
 *   - Unsanitized HTML (regular href) — fallback for legacy callsites
 */
export function installIframeLinkInterceptor(iframe, onUrl) {
  if (!iframe) return () => {}

  let disposed = false
  let attachedDoc = null
  let handler = null
  let keyHandler = null

  const extractHref = (element) => {
    if (!element?.getAttribute) return null

    // Prefer data-external-href (set by sanitizeMailHtml)
    const externalHref = element.getAttribute('data-external-href')
    const normalizedExternal = normalizeExternalUrl(externalHref)
    if (normalizedExternal) return normalizedExternal

    // Fallback: regular href (for unsanitized HTML)
    const raw = element.getAttribute('href') || ''
    const href = element.href || raw
    const normalizedHref = normalizeExternalUrl(href)
    if (normalizedHref) return normalizedHref
    return null
  }

  const findAnchorFromEvent = (event) => {
    const path = typeof event?.composedPath === 'function' ? event.composedPath() : null
    if (Array.isArray(path)) {
      for (const node of path) {
        if (!node || node === window) continue
        const el = node?.nodeType === Node.TEXT_NODE ? node.parentElement : node
        if (el?.getAttribute?.('data-external-href')) return el
        if ((el?.tagName === 'A' || el?.tagName === 'AREA') && el.getAttribute?.('href')) return el
        const clickable = el?.closest?.('[data-external-href], a[href], area[href]')
        if (clickable) return clickable
      }
    }
    const target = event?.target
    const elementTarget = target?.nodeType === Node.TEXT_NODE ? target.parentElement : target
    return elementTarget?.closest?.('[data-external-href], a[href], area[href]') || null
  }

  const tryAttach = () => {
    if (disposed) return
    const doc = iframe.contentDocument
    if (!doc || doc === attachedDoc) return

    attachedDoc = doc
    handler = (event) => {
      try {
        const anchor = findAnchorFromEvent(event)
        if (!anchor) return

        const href = extractHref(anchor)
        if (!href) return

        event.preventDefault()
        event.stopImmediatePropagation?.()
        event.stopPropagation()
        onUrl?.(href)
      } catch {
        // ignore
      }
    }

    keyHandler = (event) => {
      try {
        const key = event?.key
        if (key !== 'Enter' && key !== ' ') return
        const active = doc.activeElement
        const element = active?.closest?.('[data-external-href], a[href], area[href]') || ((active?.tagName === 'A' || active?.tagName === 'AREA') ? active : null)
        if (!element) return
        const href = extractHref(element)
        if (!href) return
        event.preventDefault()
        event.stopImmediatePropagation?.()
        event.stopPropagation()
        onUrl?.(href)
      } catch {
        // ignore
      }
    }

    doc.addEventListener('click', handler, true)
    doc.addEventListener('auxclick', handler, true)
    doc.addEventListener('mousedown', handler, true)
    doc.addEventListener('pointerdown', handler, true)
    doc.addEventListener('keydown', keyHandler, true)
  }

  tryAttach()

  return () => {
    disposed = true
    try {
      if (attachedDoc && handler) {
        attachedDoc.removeEventListener('click', handler, true)
        attachedDoc.removeEventListener('auxclick', handler, true)
        attachedDoc.removeEventListener('mousedown', handler, true)
        attachedDoc.removeEventListener('pointerdown', handler, true)
      }
      if (attachedDoc && keyHandler) {
        attachedDoc.removeEventListener('keydown', keyHandler, true)
      }
    } catch {
      // ignore
    }
  }
}
