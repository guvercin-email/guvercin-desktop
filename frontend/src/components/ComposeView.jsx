import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import Editor from '@monaco-editor/react'
import ComposeEditor from './ComposeEditor.jsx'
import ComposeIcon from './ComposeIcon.jsx'
import { buildComposePreviewDocument, escapeHtml, seedHtmlFromPlainText } from '../utils/composeHtml.js'
import { composeRecipientsToString, ensureHtmlDraftSeed } from '../utils/compose.js'
import { fetchContacts, displayNameOf } from '../utils/contactsApi.js'
import './ComposeView.css'

function createClientId(prefix) {
    if (typeof crypto !== 'undefined' && typeof crypto.randomUUID === 'function') {
        return `${prefix}-${crypto.randomUUID()}`
    }
    return `${prefix}-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`
}

function readFileAsBase64(file) {
    return new Promise((resolve, reject) => {
        const reader = new FileReader()
        reader.onload = () => {
            const result = typeof reader.result === 'string' ? reader.result : ''
            const base64 = result.includes(',') ? result.split(',')[1] : result
            resolve(base64)
        }
        reader.onerror = () => reject(reader.error || new Error('File could not be read'))
        reader.readAsDataURL(file)
    })
}

function splitRecipientDraft(value) {
    return `${value || ''}`.split(/[,\n;]+/).map((part) => part.trim()).filter(Boolean)
}

function SegmentSwitch({ value, options, onChange }) {
    return (
        <div className="cv-segment" role="tablist">
            {options.map((option) => (
                <button
                    key={option.value}
                    type="button"
                    className={`cv-segment__item ${value === option.value ? 'is-active' : ''}`}
                    onClick={() => onChange(option.value)}
                >
                    {option.label}
                </button>
            ))}
        </div>
    )
}

function ComposeAttachmentList({ attachments, onRemove }) {
    const { t } = useTranslation()
    if (attachments.length === 0) return null

    return (
        <div className="cv-attachments">
            <div className="cv-attachments__header">{t('Attachments')}</div>
            <div className="cv-attachments__list">
                {attachments.map((attachment) => (
                    <div key={attachment.id} className="cv-attachments__item">
                        <div className="cv-attachments__meta">
                            <div className="cv-attachments__name">{attachment.name}</div>
                            <div className="cv-attachments__details">
                                {attachment.mimeType} · {attachment.size || 0} B
                            </div>
                        </div>
                        <button
                            type="button"
                            className="cv-attachments__remove"
                            onClick={() => onRemove(attachment.id)}
                        >
                            {t('Remove')}
                        </button>
                    </div>
                ))}
            </div>
        </div>
    )
}

function RecipientField({ label, recipients, onChange, placeholder, trailingActions = null, suggestions = [] }) {
    const [draftToken, setDraftToken] = useState('')
    const [hoveredIndex, setHoveredIndex] = useState(null)
    const [activeSuggestion, setActiveSuggestion] = useState(0)
    const inputRef = useRef(null)

    const focusInput = useCallback(() => {
        inputRef.current?.focus()
    }, [])

    const commitRecipients = useCallback((nextRecipients) => {
        onChange(nextRecipients)
    }, [onChange])

    const finalizeToken = useCallback((rawValue = draftToken) => {
        const parts = splitRecipientDraft(rawValue)
        if (parts.length === 0) {
            setDraftToken('')
            return
        }

        commitRecipients([...recipients, ...parts])
        setDraftToken('')
    }, [commitRecipients, draftToken, recipients, setDraftToken])

    const matches = useMemo(() => draftToken.trim() ? suggestions.filter((item) => `${item.name} ${item.email}`.toLocaleLowerCase().includes(draftToken.trim().toLocaleLowerCase())).slice(0, 6) : [], [draftToken, suggestions])
    const chooseSuggestion = useCallback((item) => {
        if (!item?.email) return
        commitRecipients([...recipients, item.email])
        setDraftToken('')
        setActiveSuggestion(0)
        inputRef.current?.focus()
    }, [commitRecipients, recipients, setDraftToken, setActiveSuggestion])

    const handleKeyDown = useCallback((event) => {
        if (matches.length && event.key === 'ArrowDown') {
            event.preventDefault()
            setActiveSuggestion((index) => (index + 1) % matches.length)
            return
        }
        if (matches.length && event.key === 'ArrowUp') {
            event.preventDefault()
            setActiveSuggestion((index) => (index - 1 + matches.length) % matches.length)
            return
        }
        if (matches.length && event.key === 'Enter' && !event.shiftKey) {
            event.preventDefault()
            chooseSuggestion(matches[activeSuggestion] || matches[0])
            return
        }
        if (event.key === 'Enter' || event.key === 'Tab' || event.key === ',' || event.key === ';') {
            if (draftToken.trim()) {
                event.preventDefault()
                finalizeToken()
            }
            return
        }

        if (event.key === 'Backspace' && !draftToken) {
            event.preventDefault()
            if (recipients.length > 0) {
                // Don't drop the chip outright — pull the last one back into the
                // input as editable text so it can be corrected.
                const last = recipients[recipients.length - 1] || ''
                commitRecipients(recipients.slice(0, -1))
                setDraftToken(last)
            }
        }
    }, [activeSuggestion, chooseSuggestion, commitRecipients, draftToken, finalizeToken, matches, recipients, setActiveSuggestion])

    const handleChange = useCallback((event) => {
        const nextValue = event.target.value
        if (/[,\n;]$/.test(nextValue) || /\s$/.test(nextValue)) {
            finalizeToken(nextValue)
            return
        }

        setDraftToken(nextValue)
    }, [finalizeToken, setDraftToken])

    const handleBlur = useCallback(() => {
        if (draftToken.trim()) {
            finalizeToken()
        }
    }, [draftToken, finalizeToken])

    const handleChipClick = useCallback((index) => {
        // Clicking a chip deletes it and drops its address back into the input as
        // free-form text so it can be edited. Any half-typed token already in the
        // input is committed first so it isn't lost. The chip button's mousedown
        // is prevented from stealing focus, so the input never blurs mid-swap.
        const clicked = recipients[index] || ''
        const remaining = recipients.filter((_, recipientIndex) => recipientIndex !== index)
        const pending = splitRecipientDraft(draftToken)
        commitRecipients([...remaining, ...pending])
        setDraftToken(clicked)
        focusInput()
    }, [commitRecipients, draftToken, focusInput, recipients, setDraftToken])

    const handleRemove = useCallback((index) => {
        const nextRecipients = recipients.filter((_, recipientIndex) => recipientIndex !== index)
        commitRecipients(nextRecipients)
        focusInput()
    }, [commitRecipients, focusInput, recipients])

    return (
        <div className="cv-field cv-field--recipients">
            <label className="cv-label">{label}</label>
            <div className="cv-recipient-wrap" onClick={focusInput}>
                <div className="cv-recipient-list">
                    {recipients.map((recipient, index) => (
                        <button
                            key={`${recipient}-${index}`}
                            type="button"
                            className="cv-recipient-chip"
                            onMouseDown={(event) => event.preventDefault()}
                            onClick={() => handleChipClick(index)}
                            onMouseEnter={() => setHoveredIndex(index)}
                            onMouseLeave={() => setHoveredIndex((current) => (current === index ? null : current))}
                        >
                            <span className="cv-recipient-chip__label">{recipient}</span>
                            <span
                                className={`cv-recipient-chip__remove ${hoveredIndex === index ? 'is-visible' : ''}`}
                                onMouseDown={(event) => event.preventDefault()}
                                onClick={(event) => {
                                    event.stopPropagation()
                                    handleRemove(index)
                                }}
                            >
                                <ComposeIcon name="close" size={12} />
                            </span>
                        </button>
                    ))}
                    <input
                        ref={inputRef}
                        className="cv-recipient-input"
                        type="text"
                        value={draftToken}
                        placeholder={recipients.length === 0 ? placeholder : ''}
                        onChange={handleChange}
                        onKeyDown={handleKeyDown}
                        onBlur={handleBlur}
                    />
                </div>
            </div>
            {matches.length > 0 && (
                <div className="cv-recipient-suggestions" role="listbox" aria-label={`${label} suggestions`}>
                    {matches.map((item, index) => <button key={`${item.email}-${index}`} type="button" role="option" aria-selected={index === activeSuggestion} className={`cv-recipient-suggestion${index === activeSuggestion ? ' is-active' : ''}`} onMouseDown={(event) => event.preventDefault()} onClick={() => chooseSuggestion(item)}>
                        <span>{item.name || item.email}</span><small>{item.email}</small>
                    </button>)}
                </div>
            )}
            {trailingActions}
        </div>
    )
}

export default function ComposeView({ draft, onDraftChange, onSend, onDiscard, accountEmail, accountId, sending = false }) {
    const { t } = useTranslation()
    const [addressSuggestions, setAddressSuggestions] = useState([])
    useEffect(() => {
        if (!accountId) return
        let live = true
        fetchContacts(accountId).then((cards) => {
            if (!live) return
            const contacts = (Array.isArray(cards) ? cards : []).flatMap((record) => {
                const card = record?.card || record
                return (card.emails || []).map((entry) => ({ name: displayNameOf(card), email: `${entry.value || ''}`.trim() }))
            })
            let recent = []
            try { recent = JSON.parse(localStorage.getItem(`recent_recipients_${accountId}`) || '[]') } catch { /* ignore */ }
            const unique = new Map()
            ;[...contacts, ...(Array.isArray(recent) ? recent : [])].forEach((item) => { if (item?.email && !unique.has(item.email.toLowerCase())) unique.set(item.email.toLowerCase(), item) })
            setAddressSuggestions([...unique.values()])
        }).catch(() => {})
        return () => { live = false }
    }, [accountId])
    const forwardCount = Array.isArray(draft?.forwardTargets) ? draft.forwardTargets.length : 0
    const isForwardMode = forwardCount > 0
    const bulkReplyCount = Array.isArray(draft?.bulkReplyTargets) ? draft.bulkReplyTargets.length : 0
    const isBulkReplyMode = bulkReplyCount > 0
    const recipientCount = (draft?.toRecipients || []).length + (draft?.ccRecipients || []).length + (draft?.bccRecipients || []).length
    const recipients = useMemo(() => [...(draft?.toRecipients || []), ...(draft?.ccRecipients || []), ...(draft?.bccRecipients || [])], [draft?.bccRecipients, draft?.ccRecipients, draft?.toRecipients])
    const manualAttachments = (draft?.attachments || []).filter((attachment) => attachment.disposition !== 'inline')
    const fileInputRef = useRef(null)
    const inlineImageInputRef = useRef(null)
    const monacoRef = useRef(null)
    const [showForwardAdvanced, setShowForwardAdvanced] = useState(false)

    const patchDraft = useCallback((patch) => {
        if (!onDraftChange) return
        if (typeof patch === 'function') {
            onDraftChange((currentDraft) => patch(currentDraft || {}))
            return
        }
        onDraftChange((currentDraft) => ({ ...(currentDraft || {}), ...patch }))
    }, [onDraftChange])

    const patchRecipients = useCallback((field, recipients) => {
        const nextRecipients = recipients.map((entry) => `${entry || ''}`.trim()).filter(Boolean)
        const textValue = composeRecipientsToString(nextRecipients)
        const patch = { [`${field}Recipients`]: nextRecipients, [field]: textValue }

        if (field === 'cc') {
            patch.showCc = nextRecipients.length > 0 || draft?.showCc
        }
        if (field === 'bcc') {
            patch.showBcc = nextRecipients.length > 0 || draft?.showBcc
        }

        patchDraft(patch)
    }, [draft?.showBcc, draft?.showCc, patchDraft])

    const handleSend = useCallback(async () => {
        if (!isBulkReplyMode && recipientCount === 0) return
        const invalid = recipients.find((entry) => !/^[^\s@<>;,]+@[^\s@<>;,]+\.[^\s@<>;,]+$/.test(`${entry || ''}`.trim()))
        if (invalid) {
            window.alert(t('Check the recipient address: {{address}}', { address: invalid }))
            return
        }
        const body = `${draft?.plainBody || ''} ${draft?.htmlBody || ''}`.replace(/<[^>]*>/g, ' ').toLocaleLowerCase()
        const mentionsAttachment = /\b(attach(?:ed|ment)?|enclos(?:ed|ure)|dosya(?:yı|yı)? ek|ekli dosya|ekte)\b/i.test(body)
        if (mentionsAttachment && !(draft?.attachments || []).some((attachment) => attachment.disposition !== 'inline')) {
            if (!window.confirm(t('Your message mentions an attachment, but no file is attached. Send anyway?'))) return
        }
        if (!(draft?.subject || '').trim() && !window.confirm(t('The subject is empty. Send anyway?'))) return
        const sent = await onSend?.({
            ...draft,
            from: accountEmail || '',
        })
        if (sent && accountId) {
            try {
                const key = `recent_recipients_${accountId}`
                const previous = JSON.parse(localStorage.getItem(key) || '[]')
                const current = recipients.map((email) => ({ name: '', email: `${email}`.trim() }))
                const seen = new Set()
                localStorage.setItem(key, JSON.stringify([...current, ...(Array.isArray(previous) ? previous : [])].filter((entry) => {
                    const normalized = `${entry.email || ''}`.toLowerCase()
                    if (!normalized || seen.has(normalized)) return false
                    seen.add(normalized)
                    return true
                }).slice(0, 30)))
            } catch { /* recent recipient history is best effort */ }
        }
    }, [accountEmail, accountId, draft, isBulkReplyMode, onSend, recipientCount, recipients, t])

    const previewDocument = useMemo(
        () => buildComposePreviewDocument(draft?.htmlBody || ''),
        [draft?.htmlBody],
    )

    // The normal writing surface ("rich") is a WYSIWYG editor that sends HTML,
    // Gmail/Outlook-style. The "html" view is the raw HTML source editor.
    const bodyView = (draft?.format === 'html' && (draft?.htmlMode === 'edit' || draft?.htmlMode === 'preview'))
        ? 'html'
        : 'rich'

    const richInitialContent = useMemo(() => {
        const html = `${draft?.htmlBody || ''}`
        if (html.trim()) return html
        return seedHtmlFromPlainText(draft?.plainBody || '')
    }, [draft?.htmlBody, draft?.plainBody])

    const handleRichChange = useCallback((html) => {
        // Rich edits promote the draft to HTML so formatting survives sending.
        patchDraft({ htmlBody: html, format: 'html', htmlMode: 'rich' })
    }, [patchDraft])

    const handleBodyViewChange = useCallback((nextView) => {
        if (nextView === 'html') {
            const seeded = ensureHtmlDraftSeed(draft)
            onDraftChange?.({
                ...seeded,
                format: 'html',
                htmlMode: 'edit',
            })
            return
        }

        // Switch to the rich WYSIWYG surface. Seed HTML from plain text if the
        // draft has no HTML body yet so nothing the user typed is lost.
        const seeded = ensureHtmlDraftSeed(draft)
        onDraftChange?.({
            ...seeded,
            format: 'html',
            htmlMode: 'rich',
        })
    }, [draft, onDraftChange])

    const patchForwardOptions = useCallback((patch) => {
        const current = draft?.forwardOptions || { subjectPrefix: 'Fwd:', forwardStyle: 'copy' }
        patchDraft({ forwardOptions: { ...current, ...patch } })
    }, [draft?.forwardOptions, patchDraft])

    const patchBulkReplyOptions = useCallback((patch) => {
        const current = draft?.bulkReplyOptions || { mode: 'reply', includeQuote: true }
        patchDraft({ bulkReplyOptions: { ...current, ...patch } })
    }, [draft?.bulkReplyOptions, patchDraft])

    const handleFilesSelected = useCallback(async (fileList, disposition) => {
        const files = Array.from(fileList || [])
        if (files.length === 0) return

        const prepared = await Promise.all(files.map(async (file) => {
            const base64 = await readFileAsBase64(file)
            const contentId = disposition === 'inline' ? createClientId('cid') : undefined
            return {
                id: createClientId('attachment'),
                name: file.name || 'attachment',
                mimeType: file.type || 'application/octet-stream',
                size: file.size || 0,
                base64,
                disposition,
                contentId,
                source: disposition === 'inline' ? 'html-inline' : 'manual',
            }
        }))

        if (disposition !== 'inline') {
            patchDraft((currentDraft) => ({
                ...currentDraft,
                attachments: [...(currentDraft?.attachments || []), ...prepared],
            }))
            return
        }

        const snippets = prepared
            .map((attachment) => `<img src="cid:${attachment.contentId}" alt="${escapeHtml(attachment.name)}">`)
            .join('\n')

        const editor = monacoRef.current
        let nextHtmlBody = ''
        if (editor) {
            const selection = editor.getSelection()
            const range = selection || undefined
            editor.executeEdits('inline-image', [{
                range,
                text: snippets,
                forceMoveMarkers: true,
            }])
            editor.focus()
            nextHtmlBody = editor.getValue()
        }

        patchDraft((currentDraft) => ({
            ...currentDraft,
            attachments: [...(currentDraft?.attachments || []), ...prepared],
            htmlBody: editor
                ? nextHtmlBody
                : `${currentDraft?.htmlBody || ''}${currentDraft?.htmlBody ? '\n' : ''}${snippets}`,
        }))
    }, [patchDraft])

    const handleAttachmentInput = useCallback(async (event, disposition) => {
        const input = event.target
        try {
            await handleFilesSelected(input.files, disposition)
        } finally {
            input.value = ''
        }
    }, [handleFilesSelected])

    const handleRemoveAttachment = useCallback((attachmentId) => {
        patchDraft({
            attachments: (draft?.attachments || []).filter((attachment) => attachment.id !== attachmentId),
        })
    }, [draft?.attachments, patchDraft])

    return (
        <div className="compose-view">
            <input
                ref={fileInputRef}
                type="file"
                multiple
                hidden
                onChange={(event) => handleAttachmentInput(event, 'attachment')}
            />
            <input
                ref={inlineImageInputRef}
                type="file"
                accept="image/*"
                multiple
                hidden
                onChange={(event) => handleAttachmentInput(event, 'inline')}
            />

            <div className="cv-toolbar">
                <div className="cv-toolbar__left">
                    <SegmentSwitch
                        value={bodyView}
                        onChange={handleBodyViewChange}
                        options={[
                            { value: 'rich', label: t('Text') },
                            { value: 'html', label: t('HTML') },
                        ]}
                    />
                    {bodyView === 'html' && (
                        <SegmentSwitch
                            value={draft?.htmlMode || 'edit'}
                            onChange={(value) => patchDraft({ htmlMode: value })}
                            options={[
                                { value: 'edit', label: t('Edit') },
                                { value: 'preview', label: t('Preview') },
                            ]}
                        />
                    )}
                </div>
                <div className="cv-toolbar__right">
                    <button
                        type="button"
                        className="cv-toolbar-btn"
                        onClick={() => fileInputRef.current?.click()}
                    >
                        <ComposeIcon name="attachment" />
                        {t('Add attachment')}
                    </button>
                    {bodyView === 'html' && draft?.htmlMode === 'edit' && (
                        <button
                            type="button"
                            className="cv-toolbar-btn"
                            onClick={() => inlineImageInputRef.current?.click()}
                        >
                            <ComposeIcon name="image" />
                            {t('Insert image')}
                        </button>
                    )}
                </div>
            </div>

            {isForwardMode && (
                <div className="cv-banner">
                <div className="cv-banner__title">{t('Forwarding message')}</div>
                    <div className="cv-banner__row">
                        <button
                            type="button"
                            className="cv-toggle-btn"
                            onClick={() => setShowForwardAdvanced((prev) => !prev)}
                        >
                            {showForwardAdvanced ? t('Hide advanced') : t('Advanced')}
                        </button>
                    </div>
                    {showForwardAdvanced && (
                        <div className="cv-forward-advanced">
                            <label className="cv-banner__label">
                                {t('Style')}
                                <select
                                    className="cv-input"
                                    value={draft?.forwardOptions?.forwardStyle || 'copy'}
                                    onChange={(e) => patchForwardOptions({ forwardStyle: e.target.value })}
                                >
                                    <option value="copy">{t('Inline (body + attachments)')}</option>
                                    <option value="eml">{t('Attach original email')}</option>
                                </select>
                            </label>
                            <label className="cv-banner__label">
                                {t('Subject prefix')}
                                <input
                                    className="cv-input"
                                    type="text"
                                    value={draft?.forwardOptions?.subjectPrefix || 'Fwd:'}
                                    onChange={(e) => patchForwardOptions({ subjectPrefix: e.target.value })}
                                />
                            </label>
                        </div>
                    )}
                    <div className="cv-banner__hint">
                        {(draft?.subject || '').trim() ? t('Subject overrides the original subject.') : t('Leave Subject empty to keep the original subject.')}
                    </div>
                </div>
            )}

            {isBulkReplyMode && (
                <div className="cv-banner cv-banner--warning">
                    <div className="cv-banner__title">{t('Sending {{count}} separate replies', { count: bulkReplyCount })}</div>
                    <div className="cv-banner__row">
                        <label className="cv-banner__label">
                            {t('Mode')}
                            <select
                                className="cv-input"
                                value={draft?.bulkReplyOptions?.mode || 'reply'}
                                onChange={(e) => patchBulkReplyOptions({ mode: e.target.value })}
                            >
                                <option value="reply">Reply</option>
                            </select>
                        </label>
                        <label className="cv-banner__label cv-banner__checkbox">
                            <input
                                type="checkbox"
                                checked={draft?.bulkReplyOptions?.includeQuote !== false}
                                onChange={(e) => patchBulkReplyOptions({ includeQuote: e.target.checked })}
                            />
                            {t('Include quoted original')}
                        </label>
                    </div>
                    <div className="cv-banner__hint">
                        {t('The message you write below will be sent to each selected email as a separate reply.')}
                    </div>
                </div>
            )}

            <div className="cv-header">
                <div className="cv-field">
                    <label className="cv-label">{t('From')}</label>
                    <div className="cv-from-value">{accountEmail || '—'}</div>
                </div>
                {!isBulkReplyMode && (
                    <>
                        <RecipientField
                            label={t('To')}
                            recipients={draft?.toRecipients || []}
                            onChange={(recipients) => patchRecipients('to', recipients)}
                            placeholder="recipient@example.com"
                            suggestions={addressSuggestions}
                            trailingActions={(
                                <>
                                    {!draft?.showCc && (
                                        <button type="button" className="cv-toggle-btn" onClick={() => patchDraft({ showCc: true })}>CC</button>
                                    )}
                                    {!draft?.showBcc && (
                                        <button type="button" className="cv-toggle-btn" onClick={() => patchDraft({ showBcc: true })}>BCC</button>
                                    )}
                                </>
                            )}
                        />
                        {draft?.showCc && (
                            <RecipientField
                                label={t('CC')}
                                recipients={draft?.ccRecipients || []}
                                onChange={(recipients) => patchRecipients('cc', recipients)}
                                placeholder="cc@example.com"
                                suggestions={addressSuggestions}
                            />
                        )}
                        {draft?.showBcc && (
                            <RecipientField
                                label={t('BCC')}
                                recipients={draft?.bccRecipients || []}
                                onChange={(recipients) => patchRecipients('bcc', recipients)}
                                placeholder="bcc@example.com"
                                suggestions={addressSuggestions}
                            />
                        )}
                    </>
                )}
                {!isBulkReplyMode && (
                    <div className="cv-field">
                        <label className="cv-label">{t('Subject')}</label>
                        <input
                            className="cv-input"
                            type="text"
                            placeholder={isForwardMode ? 'Optional subject override' : 'Subject'}
                            value={draft?.subject || ''}
                            onChange={(e) => patchDraft({ subject: e.target.value })}
                        />
                    </div>
                )}
            </div>

            <div className="cv-editor-wrap">
                {bodyView === 'rich' ? (
                    <ComposeEditor
                        initialContent={richInitialContent}
                        onChange={handleRichChange}
                        toolbarVisible={false}
                    />
                ) : draft?.htmlMode === 'preview' ? (
                    <iframe
                        title="compose-preview"
                        className="cv-preview-frame"
                        /* No `allow-scripts`, so nothing in the previewed HTML
                           can run. `allow-same-origin` only lets the app reach
                           into the frame — which is what installs the
                           context-menu guard from main.jsx inside it, so the
                           preview behaves like the rest of the app on every
                           platform instead of showing the webview's own menu. */
                        sandbox="allow-same-origin"
                        srcDoc={previewDocument}
                    />
                ) : (
                    <Editor
                        height="100%"
                        defaultLanguage="html"
                        value={draft?.htmlBody || ''}
                        onChange={(value) => patchDraft({ htmlBody: value || '' })}
                        onMount={(editor) => {
                            monacoRef.current = editor
                        }}
                        options={{
                            automaticLayout: true,
                            wordWrap: 'on',
                            minimap: { enabled: false },
                            lineNumbers: 'on',
                            scrollBeyondLastLine: false,
                            tabSize: 2,
                        }}
                    />
                )}
            </div>

            <ComposeAttachmentList attachments={manualAttachments} onRemove={handleRemoveAttachment} />

            <div className="cv-actions">
                <button
                    type="button"
                    className="cv-send-btn"
                    onClick={handleSend}
                    disabled={sending || (!isBulkReplyMode && recipientCount === 0)}
                >
                    {sending ? <><ComposeIcon name="clock" /> {t('Sending...')}</>
                        : isBulkReplyMode
                            ? <><ComposeIcon name="reply" /> {t('Send replies ({{count}})', { count: bulkReplyCount })}</>
                            : isForwardMode
                                ? <><ComposeIcon name="forward" /> {t('Forward')}</>
                                : <><ComposeIcon name="send" /> {t('Send')}</>}
                </button>
                <button
                    type="button"
                    className="cv-discard-btn"
                    onClick={onDiscard}
                >
                    <ComposeIcon name="discard" /> {t('Discard')}
                </button>
            </div>
        </div>
    )
}
