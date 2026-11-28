import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { googleCalendarPermissions } from '../utils/calendarApi.js'

const ROLES = {
    freeBusyReader: 'Free/busy only',
    reader: 'Read events',
    writer: 'Edit events',
    writerWithoutPrivateAccess: 'Edit non-private events',
    owner: 'Manage calendar',
    none: 'No access',
}

export default function CalendarSharing({ accountId, calendar, onClose, onSendCopy }) {
    const { t } = useTranslation()
    const [rules, setRules] = useState(null)
    const [email, setEmail] = useState('')
    const [role, setRole] = useState('reader')
    const [busy, setBusy] = useState(false)
    const [error, setError] = useState('')
    const [revision, setRevision] = useState(0)

    useEffect(() => {
        if (!calendar.googleCalendar) return
        const controller = new AbortController()
        googleCalendarPermissions(accountId, calendar.calendarId, { signal: controller.signal })
            .then(setRules).catch((error) => { if (!controller.signal.aborted) setError(error.message) })
        return () => controller.abort()
    }, [accountId, calendar.calendarId, calendar.googleCalendar, revision])

    async function changeAccess(options) {
        setBusy(true)
        setError('')
        try {
            await googleCalendarPermissions(accountId, calendar.calendarId, options)
            if (options.grant) setEmail('')
            setRevision((current) => current + 1)
        } catch (error) {
            setError(error.message)
        } finally {
            setBusy(false)
        }
    }

    return (
        <div className="cal-modal-overlay" onClick={onClose}>
            <div className="cal-editor" role="dialog" aria-modal="true" aria-label={t('Share calendar')}
                onClick={(event) => event.stopPropagation()}>
                <div className="cal-editor-topbar">
                    <div className="cal-editor-title">{t('Share calendar')} · {calendar.name}</div>
                    <button type="button" className="cal-btn" onClick={onClose}>{t('Close')}</button>
                </div>
                <div className="cal-editor-body">
                    <button type="button" className="cal-btn" onClick={onSendCopy}>{t('Send an ICS copy')}</button>
                    {calendar.googleCalendar && <>
                        {rules === null && !error && <p role="status">{t('Loading…')}</p>}
                        {rules?.length > 0 && <ul>
                            {rules.map((rule) => <li key={rule.id}>
                                {rule.scope.value || rule.scope.type} · {t(ROLES[rule.role] || rule.role)}
                                {rule.scope.type === 'user' && rule.role !== 'owner' && (
                                    <button type="button" className="cal-btn" disabled={busy} onClick={() => {
                                        if (window.confirm(t('Remove calendar access for {{email}}?', { email: rule.scope.value }))) {
                                            void changeAccess({ ruleId: rule.id })
                                        }
                                    }}>{t('Remove access')}</button>
                                )}
                            </li>)}
                        </ul>}
                        {rules && <form className="cal-form-row2" onSubmit={(event) => {
                            event.preventDefault()
                            void changeAccess({ grant: { email, role } })
                        }}>
                            <label className="cal-labeled">
                                <span>{t('Email')}</span>
                                <input className="cal-input" type="email" value={email} required onChange={(event) => setEmail(event.target.value)} />
                            </label>
                            <label className="cal-labeled">
                                <span>{t('Access level')}</span>
                                <select className="cal-select" value={role} onChange={(event) => setRole(event.target.value)}>
                                    {['freeBusyReader', 'reader', 'writer'].map((key) => <option key={key} value={key}>{t(ROLES[key])}</option>)}
                                </select>
                            </label>
                            <button type="submit" className="cal-btn cal-btn--primary" disabled={busy}>{t('Save access')}</button>
                        </form>}
                    </>}
                    {error && <p role="alert">{error}</p>}
                </div>
            </div>
        </div>
    )
}
