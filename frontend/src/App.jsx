import { lazy, Suspense, useEffect, useState } from 'react'
import { Routes, Route, Navigate, useLocation, useNavigate } from 'react-router-dom'
import { apiUrl, apiReady } from './utils/api'
import LoginPage from './pages/LoginPage.jsx'
import OfflineSetupPage from './pages/OfflineSetupPage.jsx'
import NotAuthPage from './pages/NotAuthPage.jsx'
import DashboardPage from './pages/DashboardPage.jsx'
import AccountSelectionPage from './pages/AccountSelectionPage.jsx'

// Split out of the main bundle. The detached windows are small popups that the
// main window never renders (and vice versa), and the settings screens are only
// reached after the dashboard is already up — none of them need to be parsed at
// startup.
const DetachedMailWindow = lazy(() => import('./pages/DetachedMailWindow.jsx'))
const DetachedComposeWindow = lazy(() => import('./pages/DetachedComposeWindow.jsx'))
// Imported for its side effect: the module initialises i18next on load.
import './i18n'
import { useTranslation } from 'react-i18next'
import { hydrateAccountSession } from './utils/accountStorage.js'
import { initMailtoInbox } from './utils/mailtoInbox.js'
import { initEmlInbox } from './utils/emlInbox.js'
import { initAttachmentInbox } from './utils/attachmentInbox.js'
import { initNotifications } from './utils/notifications.js'
const ThemePage = lazy(() => import('./pages/ThemePage.jsx'))
const SettingsPage = lazy(() => import('./pages/SettingsPage.jsx'))
const AccountSettingsPage = lazy(() => import('./pages/AccountSettingsPage.jsx'))

function getDetachedHint() {
  try {
    const hint = typeof window !== 'undefined' ? window.__GUV_DETACHED__ : null
    if (!hint || typeof hint !== 'object') return null
    const kind = typeof hint.kind === 'string' ? hint.kind : ''
    const label = typeof hint.label === 'string' ? hint.label : ''
    if (!kind && !label) return null
    return { kind, label }
  } catch {
    return null
  }
}

function App() {
  const location = useLocation()
  const [windowLabel, setWindowLabel] = useState(() => getDetachedHint()?.label || '')
  const detachedHint = getDetachedHint()

  const isMailWindow = (
    detachedHint?.kind === 'mail'
    || windowLabel === 'mail'
    || windowLabel.startsWith('mail-')
    || windowLabel.startsWith('import-mail-')
  )

  const isComposeWindow = (
    detachedHint?.kind === 'compose'
    || windowLabel === 'compose'
    || windowLabel.startsWith('compose-')
  )

  // Start listening for `mailto:` and file attachment deep links as early as possible
  // so links received during a cold start are queued until the dashboard can handle them.
  // Only the main window installs the listener to avoid duplicate handling.
  useEffect(() => {
    if (isMailWindow || isComposeWindow) return
    initMailtoInbox()
    initEmlInbox()
    initAttachmentInbox()
    initNotifications()
  }, [isMailWindow, isComposeWindow])

  useEffect(() => {
    let active = true
    const detectWindowLabel = async () => {
      try {
        const { getCurrentWebviewWindow } = await import('@tauri-apps/api/webviewWindow')
        const label = getCurrentWebviewWindow().label
        if (active) setWindowLabel(label)
      } catch {
        
      }
    }
    detectWindowLabel()
    return () => {
      active = false
    }
  }, [])

  useEffect(() => {
    const path = location.pathname
    const isDetachedWindow = isMailWindow || isComposeWindow

    if (!isDetachedWindow && (path === '/login' || path === '/')) {
      localStorage.removeItem('temp_account_form')
      localStorage.removeItem('temp_font')
      localStorage.removeItem('temp_theme_mode')
      localStorage.removeItem('temp_theme_name')
      localStorage.removeItem('temp_offline_config')
    }

    const tempFont = localStorage.getItem('temp_font')
    const savedFont = localStorage.getItem('font')

    const defaultFontStack = '"Hanken Grotesk", "Helvetica Neue", Helvetica, Arial, sans-serif'
    const chosenFont = tempFont || savedFont
    let fontToUse = chosenFont ? `"${chosenFont}", ${defaultFontStack}` : defaultFontStack

    if (
      path.startsWith('/dashboard')
      || isMailWindow
      || isComposeWindow
      || isDetachedWindow
    ) {
      
      document.body.style.padding = '0'
      document.body.style.margin = '0'
      document.body.style.overflow = 'hidden'
    } else {
      document.body.style.padding = ''
      document.body.style.margin = ''
      document.body.style.overflow = ''
    }

    document.body.style.fontFamily = fontToUse
  }, [location, windowLabel, isMailWindow, isComposeWindow])

  if (isMailWindow) {
    return (
      <Suspense fallback={null}>
        <DetachedMailWindow initialLabel={windowLabel} />
      </Suspense>
    )
  }

  if (isComposeWindow) {
    return (
      <Suspense fallback={null}>
        <DetachedComposeWindow initialLabel={windowLabel} />
      </Suspense>
    )
  }

  return (
    <Suspense fallback={null}>
      <Routes>
        <Route path="/" element={<StartupRouter />} />
        <Route path="/index.html" element={<Navigate to="/" replace />} />
        <Route path="/account-select" element={<AccountSelectionPage />} />
        <Route path="/login" element={<LoginPage />} />
        <Route path="/theme" element={<ThemePage />} />
        <Route path="/offline-setup" element={<OfflineSetupPage />} />
        <Route path="/not_auth" element={<NotAuthPage />} />
        <Route path="/dashboard" element={<DashboardPage />} />
        <Route path="/settings" element={<SettingsPageWrapper />} />
        <Route path="/account-settings" element={<AccountSettingsPage />} />
        <Route path="*" element={<Navigate to="/" replace />} />
      </Routes>
    </Suspense>
  )
}

export default App

function StartupRouter() {
  const { t } = useTranslation()
  const navigate = useNavigate()
  const [error, setError] = useState(null)

  useEffect(() => {
    let active = true
    let retryTimer

    const fetchAccounts = async () => {
      if (!active) return
      try {
        setError(null)

        await apiReady
        const response = await fetch(apiUrl('/api/auth/accounts'))
        if (!response.ok) {
          throw new Error('Failed to load accounts')
        }
        const data = await response.json()
        if (!active) return

        const accounts = Array.isArray(data.accounts) ? data.accounts : []
        if (accounts.length === 0) {
          navigate('/login', { replace: true })
        } else if (accounts.length === 1) {
          // Only one registered account: there is nothing to choose, so sign in
          // with it directly instead of showing the selection screen.
          hydrateAccountSession(accounts[0])
          navigate('/dashboard', { replace: true })
        } else {
          // Always go to account selection first.
          // This satisfies the user's request: "don't ask password on app start, ask when entering account".
          navigate('/account-select', { replace: true })
        }
      } catch {
        if (!active) return

        setError(t('Unable to load accounts. Retrying...'))
        retryTimer = setTimeout(fetchAccounts, 2000)
      }
    }

    fetchAccounts()
    return () => {
      active = false
      if (retryTimer) clearTimeout(retryTimer)
    }
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])


  if (error) {
    return (
      <div className="startup-router">
        <p>{error}</p>
        <div className="startup-router__actions">
          <button type="button" onClick={() => window.location.reload()}>
            {t('Try again')}
          </button>
          <button type="button" onClick={() => navigate('/login', { replace: true })}>
            {t('Back to Login')}
          </button>
        </div>
      </div>
    )
  }

  return (
    <div className="startup-router">
      <p>{t('Checking registered accounts...')}</p>
    </div>
  )
}

function SettingsPageWrapper() {
  const navigate = useNavigate()
  return <SettingsPage onClose={() => navigate(-1)} accountId={null} />
}
