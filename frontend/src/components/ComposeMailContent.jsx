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
    accountEmail,
    accountId,
    sending = false,
    showTopDiscard = true,
}) {
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
                    onDraftChange={onDraftChange}
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
