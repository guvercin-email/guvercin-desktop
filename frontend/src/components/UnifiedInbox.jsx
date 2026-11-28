import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { apiUrl } from '../utils/api.js'
import { mailKey } from '../utils/mailIdentity.js'

const PAGE_SIZE = 50
const rowKey = (mail) => JSON.stringify([mail.accountId, mailKey(mail)])
const actions = [
    ['mark_read', 'Mark as read'],
    ['mark_unread', 'Mark as unread'],
    ['star', 'Star'],
    ['unstar', 'Unstar'],
    ['archive', 'Archive'],
    ['spam', 'Spam'],
    ['trash', 'Move to Trash'],
]
const standardFolders = {
    archive: /^(archive|arşiv|\[gmail\]\/all mail)$/i,
    spam: /^(spam|junk|junk e-mail|\[gmail\]\/spam)$/i,
    trash: /^(trash|deleted items|deleted messages|çöp kutusu|\[gmail\]\/trash)$/i,
}

async function request(path, options) {
    const response = await fetch(apiUrl(path), { cache: 'no-store', ...options })
    if (!response.ok) throw new Error(`HTTP ${response.status}`)
    return response.json()
}

async function readAccountInbox(account, page, signal) {
    const mails = []
    let hasMore = false
    let mailboxRevision
    for (let index = 1; index <= page; index += 1) {
        const data = await request(`/api/offline/${account.account_id}/local-list?mailbox=INBOX&page=${index}&per_page=${PAGE_SIZE}`, { signal })
        if (index > 1 && mailboxRevision !== data.revision) throw new Error('Inbox changed during pagination')
        mailboxRevision = data.revision
        const chunk = data.mails || []
        mails.push(...chunk)
        hasMore = mails.length < data.total_count
        if (!hasMore) break
    }
    return {
        hasMore,
        mails: mails.map((mail) => ({
            ...mail, accountId: account.account_id, accountEmail: account.email_address, mailbox: 'INBOX',
        })),
    }
}

export default function UnifiedInbox({ onOpen, online }) {
    const { t } = useTranslation()
    const [page, setPage] = useState(1)
    const [revision, setRevision] = useState(0)
    const [snapshot, setSnapshot] = useState(null)
    const [refreshing, setRefreshing] = useState(false)
    const [liveFailures, setLiveFailures] = useState(0)
    const [selected, setSelected] = useState(new Set())
    const [busy, setBusy] = useState(false)
    const [actionResult, setActionResult] = useState(null)

    useEffect(() => {
        const controller = new AbortController()
        const { signal } = controller
        let reading = false
        async function readCache() {
            if (reading) return
            reading = true
            try {
                const { accounts = [] } = await request('/api/auth/accounts', { signal })
                const results = await Promise.allSettled(accounts.map((account) => readAccountInbox(account, page, signal)))
                if (signal.aborted) return
                const fulfilled = results.filter((result) => result.status === 'fulfilled').map((result) => result.value)
                const rows = fulfilled.flatMap((result) => result.mails)
                    .sort((left, right) => (Date.parse(right.date) || 0) - (Date.parse(left.date) || 0))
                setSnapshot({ rows, hasMore: fulfilled.some((result) => result.hasMore), unavailable: results.length - fulfilled.length })
            } catch {
                if (!signal.aborted) setSnapshot({ rows: [], hasMore: false, unavailable: 0, failed: true })
            } finally {
                reading = false
            }
        }
        void readCache()
        const timer = window.setInterval(readCache, 5_000)
        return () => {
            controller.abort()
            window.clearInterval(timer)
        }
    }, [page, revision])

    useEffect(() => {
        if (!online) return
        const controller = new AbortController()
        const { signal } = controller
        setRefreshing(true)
        async function refreshAll() {
            try {
                const { accounts = [] } = await request('/api/auth/accounts', { signal })
                const results = await Promise.allSettled(accounts.map(({ account_id: id }) =>
                    request(`/api/offline/${id}/sync-mailbox?mailbox=INBOX&per_page=250&force_all=true`, { method: 'POST', signal })))
                if (!signal.aborted) setLiveFailures(results.filter((result) => result.status === 'rejected').length)
            } catch {
                if (!signal.aborted) setLiveFailures(1)
            } finally {
                if (!signal.aborted) setRefreshing(false)
            }
        }
        void refreshAll()
        return () => controller.abort()
    }, [online, revision])

    const selectable = (snapshot?.rows || []).filter((mail) => mail.uid_validity > 0)
    const selectedRows = selectable.filter((mail) => selected.has(rowKey(mail)))
    const allSelected = selectable.length > 0 && selectable.length === selectedRows.length

    async function applyAction(action) {
        if (busy || !selectedRows.length) return
        setBusy(true)
        setActionResult(null)
        const byAccount = new Map()
        for (const mail of selectedRows) {
            if (!byAccount.has(mail.accountId)) byAccount.set(mail.accountId, [])
            byAccount.get(mail.accountId).push(mail)
        }
        const applied = new Map()
        try {
            await Promise.all([...byAccount].map(async ([id, mails]) => {
                let destination
                if (standardFolders[action]) {
                    try {
                        const data = await request(`/api/mail/${id}/mailboxes`)
                        const folders = (data.mailboxes || []).filter((folder) => !data.noselect?.includes(folder))
                        destination = folders.find((folder) => data.roles?.[folder] === (action === 'spam' ? 'junk' : action))
                            || folders.find((folder) => standardFolders[action].test(folder))
                        if (action === 'archive' && !destination) {
                            destination = folders.find((folder) => data.roles?.[folder] === 'all')
                        }
                    } catch { /* A move requires a verified destination on that account. */ }
                    if (!destination) return
                }
                for (const mail of mails) {
                    const key = rowKey(mail)
                    try {
                        await request(`/api/offline/${id}/actions`, {
                            method: 'POST',
                            headers: { 'Content-Type': 'application/json' },
                            body: JSON.stringify({
                                action_type: destination ? 'move' : action, target_uid: mail.id,
                                target_folder: mail.mailbox, uid_validity: mail.uid_validity,
                                payload: destination ? { destination } : {},
                            }),
                        })
                        applied.set(key, destination)
                    } catch { /* Keep the failed rows selected for retry. */ }
                }
            }))
            setSelected(new Set(selectedRows.filter((mail) => !applied.has(rowKey(mail))).map(rowKey)))
            setSnapshot((current) => current && {
                ...current,
                rows: current.rows.filter((mail) => !applied.get(rowKey(mail))).map((mail) => {
                    if (!applied.has(rowKey(mail))) return mail
                    if (action === 'star' || action === 'unstar') return { ...mail, flagged: action === 'star' }
                    return { ...mail, seen: action === 'mark_read' }
                }),
            })
            setActionResult({ queued: applied.size, failed: selectedRows.length - applied.size })
        } finally {
            setBusy(false)
        }
    }

    return (
        <section className="db-unified-inbox" aria-label={t('Unified inbox')}>
            <header>
                <h2>{t('Unified inbox')}</h2>
                <button type="button" disabled={online && refreshing} onClick={() => setRevision((value) => value + 1)}>{t('Refresh')}</button>
            </header>
            {online && refreshing && <p role="status">{t('Refreshing account inboxes…')}</p>}
            {online && !!liveFailures && <p role="status">{t('Some inboxes could only be loaded from the cache.')}</p>}
            {!snapshot && <p role="status">{t('Loading…')}</p>}
            {snapshot?.failed && <p role="alert">{t('Could not load the unified inbox.')}</p>}
            {!!snapshot?.unavailable && <p role="status">{t('Some account inboxes are unavailable.')}</p>}
            {snapshot && !snapshot.failed && !snapshot.rows.length && <p>{t('No cached inbox messages.')}</p>}
            {selectable.length > 0 && (
                <div className="db-unified-actions">
                    <label><input type="checkbox" checked={allSelected} disabled={busy}
                        onChange={() => setSelected(allSelected ? new Set() : new Set(selectable.map(rowKey)))} /> {t('Select all loaded messages')}</label>
                    {actions.map(([action, label]) => (
                        <button type="button" key={action} disabled={busy || !selectedRows.length || (!online && Boolean(standardFolders[action]))}
                            onClick={() => void applyAction(action)}>{t(label)}</button>
                    ))}
                </div>
            )}
            {actionResult && <p role="status">{t('Queued actions: {{queued}}. Could not queue: {{failed}}.', actionResult)}</p>}
            <ul>
                {snapshot?.rows.map((mail) => {
                    const key = rowKey(mail)
                    return (
                        <li key={key}>
                            <input type="checkbox" checked={selected.has(key)} disabled={busy || !(mail.uid_validity > 0)}
                                aria-label={t('Select {{subject}}', { subject: mail.subject || t('(no subject)') })}
                                onChange={(event) => {
                                    const next = new Set(selected)
                                    if (event.target.checked) next.add(key)
                                    else next.delete(key)
                                    setSelected(next)
                                }} />
                            <button type="button" onClick={() => onOpen(mail)}>
                                <span>{mail.name || mail.address}</span>
                                {mail.seen ? <span>{mail.subject || t('(no subject)')}</span> : <strong>{mail.subject || t('(no subject)')}</strong>}
                                <small>{mail.flagged ? '★ ' : ''}{mail.accountEmail}</small>
                                <small>{mail.date}</small>
                            </button>
                        </li>
                    )
                })}
            </ul>
            {snapshot?.hasMore && <button type="button" onClick={() => setPage((value) => value + 1)}>{t('Load more')}</button>}
        </section>
    )
}
