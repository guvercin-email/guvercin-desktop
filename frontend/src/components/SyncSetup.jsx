// Shared sync-setup UI for the Contacts and Todo tabs (the Calendar tab carries its
// own copy, wired to the same backend routes).
//
// Two modals, both driven entirely by props so one component serves every store:
//   • SyncChoiceModal — first-run onboarding *and* the settings screen. Single
//     choice between the provider's own cloud (Google), any standards-compliant DAV
//     server, and this-device-only. The current choice is marked, so re-opening it
//     reads as settings rather than onboarding.
//   • DavAccountModal — the CalDAV/CardDAV connection form. `getConfig`/`setConfig`
//     are passed in, so the same form saves a CalDAV or a CardDAV account; the
//     server validates the credentials before storing them, which is why a failed
//     save shows a real error instead of silently "connecting".
//
// Backends are the strings the API uses: '', 'google', 'caldav' | 'carddav', 'local'.

import React, { useCallback, useEffect, useState } from 'react'
import './SyncSetup.css'

const icon = (name) => <img src={`/img/icons/${name}.svg`} className="svg-icon-inline" alt="" />

// Short toolbar label for the current backend. `davLabel` is 'CalDAV' or 'CardDAV'.
export function syncLabel(t, backend, davLabel) {
  if (backend === 'google') return t('Sync: Google')
  if (backend === 'caldav' || backend === 'carddav') return t('Sync: {{server}}', { server: davLabel })
  if (backend === 'local') return t('Local only')
  return t('Set up sync')
}

export function SyncChoiceModal({
  t, title, intro, backend, davKey, googleAvailable, reconnecting,
  googleTitle, googleDesc, davTitle, davDesc, localTitle, localDesc,
  onGoogle, onDav, onLocal, onClose,
}) {
  const opt = (key, iconEl, optTitle, desc, onClick, disabled = false) => (
    <button className={`sync-opt${backend === key ? ' is-current' : ''}`} onClick={onClick} disabled={disabled}>
      <span className="sync-opt-icon">{iconEl}</span>
      <span className="sync-opt-text">
        <span className="sync-opt-title">{optTitle}{backend === key ? ' ✓' : ''}</span>
        <span className="sync-opt-desc">{desc}</span>
      </span>
    </button>
  )

  return (
    <div className="sync-modal-overlay" onClick={onClose}>
      <div className="sync-modal sync-modal--choice" onClick={(e) => e.stopPropagation()}>
        <div className="sync-modal-topbar">
          <div className="sync-modal-title">{title}</div>
          <div className="sync-modal-actions">
            <button className="sync-btn" onClick={onClose}>{t('Close')}</button>
          </div>
        </div>
        <div className="sync-modal-body">
          <p className="sync-hint">{intro}</p>
          <div className="sync-opts">
            {googleAvailable && opt(
              'google',
              <img src="/img/icon-google.png" alt="" width="22" height="22" />,
              reconnecting ? t('Waiting for Google…') : googleTitle,
              googleDesc,
              onGoogle,
              reconnecting,
            )}
            {opt(davKey, icon('settings'), davTitle, davDesc, onDav)}
            {opt('local', icon('offline'), localTitle, localDesc, onLocal)}
          </div>
        </div>
      </div>
    </div>
  )
}

export function DavAccountModal({
  t, title, hint, urlPlaceholder = 'https://dav.example.com/',
  getConfig, setConfig, describeResult,
  onClose, onChanged, pushToast,
}) {
  const [url, setUrl] = useState('')
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  const [hasPassword, setHasPassword] = useState(false)
  const [loading, setLoading] = useState(true)
  const [busy, setBusy] = useState(false)

  useEffect(() => {
    let alive = true
    getConfig()
      .then((c) => {
        if (!alive) return
        setUrl(c.url || '')
        // An untouched form pre-fills the login the account already knows.
        setUsername(c.username || c.suggestedUsername || '')
        setHasPassword(!!c.hasPassword)
      })
      .catch(() => {})
      .finally(() => { if (alive) setLoading(false) })
    return () => { alive = false }
  }, [getConfig])

  const configured = !!(url.trim() && username.trim() && (hasPassword || password))

  const save = useCallback(async () => {
    if (!url.trim() || !username.trim()) {
      pushToast(t('Enter the server address and username.'), 'error')
      return
    }
    if (!hasPassword && !password) {
      pushToast(t('Enter the password.'), 'error')
      return
    }
    setBusy(true)
    try {
      // Omit the password when keeping the saved one (leave it blank to preserve).
      const res = await setConfig({ url: url.trim(), username: username.trim(), password })
      pushToast(describeResult(res), 'info')
      onChanged(true)
      onClose()
    } catch (e) {
      pushToast(e.message || t('Could not connect.'), 'error')
    } finally {
      setBusy(false)
    }
  }, [url, username, password, hasPassword, setConfig, describeResult, onChanged, onClose, pushToast, t])

  const disconnect = useCallback(async () => {
    setBusy(true)
    try {
      await setConfig({ url: '', username: '', password: '' })
      pushToast(t('Disconnected.'), 'info')
      onChanged(false)
      onClose()
    } catch (e) {
      pushToast(e.message || t('Could not update the connection.'), 'error')
    } finally {
      setBusy(false)
    }
  }, [setConfig, onChanged, onClose, pushToast, t])

  return (
    <div className="sync-modal-overlay" onClick={onClose}>
      <div className="sync-modal sync-modal--form" onClick={(e) => e.stopPropagation()}>
        <div className="sync-modal-topbar">
          <div className="sync-modal-title">{title}</div>
          <div className="sync-modal-actions">
            <button className="sync-btn" onClick={onClose}>{t('Cancel')}</button>
            <button className="sync-btn sync-btn--primary" onClick={save} disabled={busy || loading}>
              {busy ? t('Connecting…') : t('Connect')}
            </button>
          </div>
        </div>

        <div className="sync-modal-body">
          <p className="sync-hint">{hint}</p>
          <label className="sync-field">
            <span>{t('Server address')}</span>
            <input className="sync-input" type="text" inputMode="url" autoFocus placeholder={urlPlaceholder}
              value={url} onChange={(e) => setUrl(e.target.value)} />
            <span className="sync-field-note">
              {t('A full URL, or just the domain (example.com) — the server is found automatically.')}
            </span>
          </label>
          <label className="sync-field">
            <span>{t('Username')}</span>
            <input className="sync-input" type="text" autoComplete="username" placeholder="you@example.com"
              value={username} onChange={(e) => setUsername(e.target.value)} />
          </label>
          <label className="sync-field">
            <span>{t('Password')}</span>
            <input className="sync-input" type="password" autoComplete="new-password"
              placeholder={hasPassword ? t('•••••••• (saved — leave blank to keep)') : t('App-specific password')}
              value={password} onChange={(e) => setPassword(e.target.value)} />
          </label>
        </div>

        <div className="sync-modal-footer">
          {configured
            ? <button className="sync-btn sync-btn--danger" onClick={disconnect} disabled={busy}>{t('Disconnect')}</button>
            : <span />}
        </div>
      </div>
    </div>
  )
}
