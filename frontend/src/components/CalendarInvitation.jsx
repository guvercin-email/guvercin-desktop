import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { apiUrl } from '../utils/api.js'
import { calendarInvitationReply, parseCalendarInvitations } from '../utils/calendarInvitation.js'

const CHOICES = [
    ['ACCEPTED', 'Accept invitation'],
    ['TENTATIVE', 'Tentative'],
    ['DECLINED', 'Decline invitation'],
]

function encodeCalendar(text) {
    return btoa(Array.from(new TextEncoder().encode(text), (byte) => String.fromCharCode(byte)).join(''))
}

export default function CalendarInvitation({ attachment, href, accountId, email }) {
    const { t } = useTranslation()
    const [loaded, setLoaded] = useState(null)
    const [opened, setOpened] = useState(false)
    const [status, setStatus] = useState({})
    const { content_type: type, data_base64: base64, filename, size } = attachment
    const available = accountId && email && (base64 || href) && size > 0 && size <= 262144
        && (type === 'text/calendar' || /\.ics$/i.test(filename))
    useEffect(() => {
        if (!available || !opened) return
        const controller = new AbortController()
        const load = async () => {
            let source
            if (base64) {
                if (base64.length > 350000) throw new Error('Invalid invitation')
                source = new TextDecoder().decode(Uint8Array.from(atob(base64), (byte) => byte.charCodeAt(0)))
            } else {
                const response = await fetch(href, { signal: controller.signal })
                if (!response.ok || Number(response.headers.get('content-length')) > 262144) throw new Error('Invalid invitation')
                source = await response.text()
            }
            if (!controller.signal.aborted) setLoaded({ base64, href, email, invitations: parseCalendarInvitations(source, email) })
        }
        load().catch(() => {
            if (!controller.signal.aborted) setLoaded({ base64, href, email, invitations: [] })
        })
        return () => controller.abort()
    }, [available, base64, email, href, opened])

    if (!available) return null
    const invitations = loaded?.base64 === base64 && loaded?.href === href && loaded?.email === email
        ? loaded.invitations : null
    const answer = async (invitation, index, choice, label) => {
        const id = JSON.stringify([accountId, invitation.uid, invitation.recurrence?.value, invitation.sequence, index])
        if (status[id] === 'sending' || status[id] === 'queued') return
        if (!window.confirm(t('Send your calendar response to {{email}}?', { email: invitation.organizer }))) return
        setStatus((previous) => ({ ...previous, [id]: 'sending' }))
        try {
            const response = await fetch(apiUrl(`/api/offline/${accountId}/actions`), {
                method: 'POST',
                headers: { 'Content-Type': 'application/json' },
                body: JSON.stringify({
                    action_type: 'send',
                    payload: {
                        from: email, to: [invitation.organizer], subject: `${t(label)} ${invitation.title}`.trim(),
                        format: 'plain', body_text: t(label),
                        attachments: [{
                            filename: 'reply.ics', content_type: 'text/calendar; method=REPLY; charset=UTF-8',
                            data_base64: encodeCalendar(calendarInvitationReply(invitation, choice)),
                        }],
                    },
                }),
            })
            if (!response.ok) throw new Error()
            setStatus((previous) => ({ ...previous, [id]: 'queued' }))
        } catch {
            setStatus((previous) => ({ ...previous, [id]: 'error' }))
        }
    }
    return (
        <details onToggle={(event) => setOpened(event.currentTarget.open)}>
            <summary className="db-attachments__link">{t('Calendar invitation')}</summary>
            {opened && (invitations === null ? <p role="status">{t('Loading…')}</p>
                : invitations.length === 0 ? <p>{t('No invitation for this account.')}</p> : invitations.map((invitation, index) => {
                const id = JSON.stringify([accountId, invitation.uid, invitation.recurrence?.value, invitation.sequence, index])
                return <div key={id}>
                    <p>{invitation.title} · {invitation.organizer}</p>
                    {CHOICES.map(([choice, label]) => <button type="button" key={choice}
                        disabled={['sending', 'queued'].includes(status[id])}
                        onClick={() => void answer(invitation, index, choice, label)}>{t(label)}</button>)}
                    {status[id] === 'queued' && <p role="status">{t('Calendar response queued.')}</p>}
                    {status[id] === 'error' && <p role="alert">{t('Could not queue the calendar response.')}</p>}
                </div>
            }))}
        </details>
    )
}
