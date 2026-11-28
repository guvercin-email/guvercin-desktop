import { useEffect, useRef } from 'react'
import { useTranslation } from 'react-i18next'
import ComposeIcon from './ComposeIcon.jsx'
import ComposeView from './ComposeView.jsx'
import { getComposeTitle } from '../utils/compose.js'

export default function ComposeMailContent({
    draft,
    onDraftChange,
    onSend,
    onDiscard,
    onOpenInTab,
    onOpenInWindow,
    onOpenLinkedWindow,
    onOpenLinkedTab,
    workspaceDraft = false,
    linkedEditor = false,
    accountEmail,
    accountId,
    sending = false,
    showTopDiscard = true,
}) {
    const { t } = useTranslation()
    const channelRef = useRef(null)
    const current = useRef({ draft, onDraftChange, onDiscard })
    useEffect(() => { current.current = { draft, onDraftChange, onDiscard } }, [draft, onDraftChange, onDiscard])
    const syncId = draft?.syncId
    useEffect(() => {
        if (!syncId || !accountId || typeof BroadcastChannel !== 'function') return
        const channel = new BroadcastChannel(`guvercin-compose-${accountId}`)
        channelRef.current = channel
        channel.onmessage = ({ data }) => {
            if (data?.type === 'close' && data.syncId === syncId && linkedEditor) {
                current.current.onDiscard?.()
            }
            if (data?.type === 'request' && data.syncId === syncId && !linkedEditor) {
                channel.postMessage({ type: 'draft', draft: current.current.draft })
            }
            if (!workspaceDraft && data?.type === 'draft' && data.draft?.syncId === syncId) {
                const next = { ...data.draft, composeSurface: current.current.draft?.composeSurface }
                current.current = { ...current.current, draft: next }
                current.current.onDraftChange?.(next)
            }
        }
        if (linkedEditor && !workspaceDraft) channel.postMessage({ type: 'request', syncId })
        return () => {
            channelRef.current = null
            channel.close()
        }
    }, [accountId, linkedEditor, syncId, workspaceDraft])

    const changeDraft = (update) => {
        const next = typeof update === 'function' ? update(current.current.draft) : update
        current.current = { ...current.current, draft: next }
        onDraftChange?.(next)
        if (next?.syncId) channelRef.current?.postMessage({ type: 'draft', draft: next, fromLinkedEditor: linkedEditor && !workspaceDraft })
    }

    return (
        <div
            className="db-mail-content"
            style={{ overflow: 'hidden' }}
        >
            <div className="db-mail-content-header">
                <div className="db-mail-content-subject">{getComposeTitle(draft)}</div>
                <div className="db-mail-content-actions">
                    {onOpenInTab && (
                        <button
                            type="button"
                            className="db-mail-action-btn cv-header-action"
                            onClick={onOpenInTab}
                            title="Open in new tab"
                            aria-label="Open in new tab"
                        >
                            <ComposeIcon name="openTab" />
                        </button>
                    )}
                    {onOpenInWindow && (
                        <button
                            type="button"
                            className="db-mail-action-btn cv-header-action"
                            onClick={onOpenInWindow}
                            title="Open in new window"
                            aria-label="Open in new window"
                        >
                            <ComposeIcon name="openWindow" />
                        </button>
                    )}
                    {onOpenLinkedTab && (
                        <button type="button" className="db-mail-action-btn cv-header-action"
                            onClick={onOpenLinkedTab} title={t('Open linked draft tab')}
                            aria-label={t('Open linked draft tab')}>
                            <ComposeIcon name="openTab" /> ⇄
                        </button>
                    )}
                    {onOpenLinkedWindow && typeof BroadcastChannel === 'function' && (
                        <button type="button" className="db-mail-action-btn cv-header-action"
                            onClick={onOpenLinkedWindow} title={t('Open linked draft window')}
                            aria-label={t('Open linked draft window')}>
                            <ComposeIcon name="openWindow" /> ⇄
                        </button>
                    )}
                    {onDiscard && showTopDiscard && (
                        <button
                            type="button"
                            className="db-mail-action-btn cv-header-action"
                            onClick={onDiscard}
                            title="Close"
                            aria-label="Close compose"
                        >
                            <ComposeIcon name="close" />
                        </button>
                    )}
                </div>
            </div>
            <div style={{ flex: 1, minHeight: 0, display: 'flex', flexDirection: 'column' }}>
                <ComposeView
                    draft={draft}
                    onDraftChange={changeDraft}
                    linkedEditor={linkedEditor}
                    linkedTab={linkedEditor && workspaceDraft}
                    onSend={onSend}
                    onDiscard={onDiscard}
                    accountEmail={accountEmail}
                    accountId={accountId}
                    sending={sending}
                />
            </div>
        </div>
    )
}
