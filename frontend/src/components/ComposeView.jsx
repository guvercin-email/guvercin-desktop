import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { useTranslation } from 'react-i18next'
import Editor from '@monaco-editor/react'
import ComposeEditor from './ComposeEditor.jsx'
import ComposeIcon from './ComposeIcon.jsx'
import { OfficeAttachmentContents } from './OfficeAttachmentPreview.jsx'
import { canPreviewOffice } from '../utils/officePreview.js'
import { buildComposePreviewDocument, escapeHtml, seedHtmlFromPlainText } from '../utils/composeHtml.js'
import { composeRecipientsToString, ensureHtmlDraftSeed } from '../utils/compose.js'
import { fetchContacts, displayNameOf } from '../utils/contactsApi.js'
import './ComposeView.css'

// The queue accepts 32 MiB JSON. Base64 expands attachments by roughly 4/3.
const ATTACHMENT_LIMIT = 20 * 1024 * 1024
const PREVIEW_IMAGE_TYPES = new Set(['image/png', 'image/jpeg', 'image/gif', 'image/webp', 'image/avif'])

function readSnippets(accountId, kind) {
    try {
        const entries = JSON.parse(localStorage.getItem(`compose_${kind}_${accountId}`) || '[]')
        return Array.isArray(entries) ? entries : []
    } catch {
        return []
    }
}

function ComposeSnippets({ accountId, draft, onApply }) {
    const { t } = useTranslation()
    const [banks, setBanks] = useState(() => ({
        templates: readSnippets(accountId, 'templates'),
        signatures: readSnippets(accountId, 'signatures'),
    }))

    const writeBank = (kind, entries) => {
        try {
            localStorage.setItem(`compose_${kind}_${accountId}`, JSON.stringify(entries))
            setBanks((previous) => ({ ...previous, [kind]: entries }))
        } catch {
            window.alert(t('Could not save the text.'))
        }
    }

    const save = (kind) => {
        const name = window.prompt(t(kind === 'templates' ? 'Template name' : 'Signature name'))?.trim()
        if (!name) return
        const signature = kind === 'signatures' ? window.prompt(t('Signature text')) : null
        if (kind === 'signatures' && !signature?.trim()) return
        const entry = kind === 'templates'
            ? { name, subject: draft?.subject || '', plainBody: draft?.plainBody || '', htmlBody: (draft?.htmlBody || '').replace(/<img\b[^>]*\bsrc=["']cid:[^"']+["'][^>]*>/gi, '') }
            : { name, plainBody: signature }
        if (banks[kind].some((item) => item.name === name) && !window.confirm(t('Replace the existing item?'))) return
        writeBank(kind, [...banks[kind].filter((item) => item.name !== name), entry])
    }

    return (
        <div className="cv-snippets">
            {!!banks.templates.length && (
                <select value="" aria-label={t('Insert quick reply')} onChange={(event) => {
                    const entry = banks.templates[Number(event.target.value)]
                    if (entry) onApply(entry, 'quick_reply')
                }}>
                    <option value="" disabled>{t('Insert quick reply')}</option>
                    {banks.templates.map((entry, index) => <option key={entry.name} value={index}>{entry.name}</option>)}
                </select>
            )}
            {Object.entries(banks).map(([kind, entries]) => (
                <div key={kind} className="cv-snippets__group">
                    <select value="" aria-label={t(kind === 'templates' ? 'Templates' : 'Signatures')} onChange={(event) => {
                        const entry = entries[Number(event.target.value)]
                        if (entry) onApply(entry, kind)
                    }}>
                        <option value="" disabled>{t(kind === 'templates' ? 'Templates' : 'Signatures')}</option>
                        {entries.map((entry, index) => <option key={entry.name} value={index}>{entry.name}</option>)}
                    </select>
                    <button type="button" onClick={() => save(kind)}>
                        {t(kind === 'templates' ? 'Save as template' : 'New signature')}
                    </button>
                    {!!entries.length && <button type="button" onClick={() => {
                        const name = window.prompt(t(kind === 'templates' ? 'Template name' : 'Signature name'))?.trim()
                        if (name && entries.some((entry) => entry.name === name) && window.confirm(t('Delete the saved item?'))) {
                            writeBank(kind, entries.filter((entry) => entry.name !== name))
                        }
                    }}>{t(kind === 'templates' ? 'Delete template' : 'Delete signature')}</button>}
                </div>
            ))}
        </div>
    )
}

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

const PREVIEW_TEXT_TYPES = new Set(['application/json', 'application/xml', 'application/x-yaml', 'application/yaml'])
const isTextPreview = (type) => type?.startsWith('text/') || PREVIEW_TEXT_TYPES.has(type)
const canPreviewAttachment = ({ base64, mimeType, name }) => base64 &&
    (PREVIEW_IMAGE_TYPES.has(mimeType) || mimeType === 'application/pdf' || isTextPreview(mimeType) || canPreviewOffice(name))

function AttachmentPreview({ attachment }) {
    const { t } = useTranslation()
    const [pdfUrl, setPdfUrl] = useState('')
    const { base64, mimeType, name } = attachment
    useEffect(() => {
        if (mimeType !== 'application/pdf') return
        let url
        try {
            const bytes = Uint8Array.from(atob(base64), (value) => value.charCodeAt(0))
            url = URL.createObjectURL(new Blob([bytes], { type: mimeType }))
            setPdfUrl(url)
        } catch { /* The error message below handles invalid attachments. */ }
        return () => { if (url) URL.revokeObjectURL(url) }
    }, [base64, mimeType])

    if (PREVIEW_IMAGE_TYPES.has(mimeType)) {
        return <img className="cv-attachments__preview" src={`data:${mimeType};base64,${base64}`} alt={name} />
    }
    if (mimeType === 'application/pdf' && pdfUrl) {
        return (
            <div className="cv-attachments__preview-document">
                <iframe title={name} src={pdfUrl} sandbox="" />
                <a href={pdfUrl} download={name}>{t('Download PDF')}</a>
            </div>
        )
    }
    if (canPreviewOffice(name)) return <OfficeAttachmentContents name={name} base64={base64} />
    if (isTextPreview(mimeType)) {
        let text
        try {
            const sample = atob(base64.slice(0, 131072))
            text = new TextDecoder().decode(Uint8Array.from(sample, (value) => value.charCodeAt(0)))
        } catch { /* The error message below handles invalid attachments. */ }
        if (text != null) {
            return <pre className="cv-attachments__preview-document">{text}{base64.length > 131072 ? '\n…' : ''}</pre>
        }
    }
    return <div role="status">{t('Preview unavailable.')}</div>
}

function SenderAliases({ accountId, accountEmail, value, onChange }) {
    const { t } = useTranslation()
    const key = `compose_sender_aliases_${accountId}`
    const [aliases, setAliases] = useState(() => readSnippets(accountId, 'sender_aliases')
        .filter((entry) => typeof entry === 'string'))
    const save = (next) => {
        try {
            localStorage.setItem(key, JSON.stringify(next))
            setAliases(next)
        } catch {
            window.alert(t('Could not save the sender address.'))
        }
    }
    const current = (value || '').trim()
    const existing = aliases.find((address) => address.toLowerCase() === current.toLowerCase())
    const alternate = current && current.toLowerCase() !== (accountEmail || '').toLowerCase()
    return (
        <div className="cv-snippets__group">
            <input className="cv-input" type="email" aria-label={t('From')}
                title={t('Use an address authorized by your mail provider.')}
                placeholder={accountEmail || ''} value={value || ''} onChange={(event) => onChange(event.target.value)} />
            {!!aliases.length && <select aria-label={t('Saved sender addresses')} value={existing || (alternate ? '' : 'default')}
                onChange={(event) => onChange(event.target.value === 'default' ? accountEmail || '' : event.target.value)}>
                <option value="" disabled>{t('Saved sender addresses')}</option>
                <option value="default">{accountEmail || t('Default sender')}</option>
                {aliases.map((address) => <option key={address} value={address}>{address}</option>)}
            </select>}
            {alternate && !existing && <button type="button" onClick={() => {
                if (!/^[^\s@<>;,]+@[^\s@<>;,]+\.[^\s@<>;,]+$/.test(current)) {
                    window.alert(t('Check the sender address.'))
                    return
                }
                save([...aliases, current])
            }}>{t('Save sender address')}</button>}
            {existing && <button type="button" onClick={() => save(aliases.filter((address) => address !== existing))}>
                {t('Remove saved sender')}
            </button>}
        </div>
    )
}

function ComposeAttachmentList({ attachments, onRemove }) {
    const { t } = useTranslation()
    const [previewId, setPreviewId] = useState(null)
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
                        {canPreviewAttachment(attachment) && (
                            <button type="button" className="cv-attachments__remove"
                                aria-expanded={previewId === attachment.id}
                                onClick={() => setPreviewId(previewId === attachment.id ? null : attachment.id)}>
                                {t('Preview')}
                            </button>
                        )}
                        <button
                            type="button"
                            className="cv-attachments__remove"
                            onClick={() => onRemove(attachment.id)}
                        >
                            {t('Remove')}
                        </button>
                        {previewId === attachment.id && canPreviewAttachment(attachment) && (
                            <AttachmentPreview attachment={attachment} />
                        )}
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

export default function ComposeView({ draft, onDraftChange, onSend, onDiscard, accountEmail, accountId, sending = false, linkedEditor = false, linkedTab = false }) {
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

    const [scheduleDate, setScheduleDate] = useState('')
    const [requestReceipt, setRequestReceipt] = useState(false)
    const [requestDeliveryReceipt, setRequestDeliveryReceipt] = useState(false)
    const handleSend = useCallback(async () => {
        const scheduled = scheduleDate && !isBulkReplyMode && !isForwardMode ? new Date(scheduleDate) : null
        if (scheduled && !(scheduled.getTime() > Date.now())) {
            window.alert(t('Choose a future sending time.'))
            return
        }
        if ((draft?.attachments || []).reduce((total, attachment) =>
            total + Math.max(attachment.size || 0, (attachment.base64 || '').length * 3 / 4), 0) > ATTACHMENT_LIMIT) {
            window.alert(t('Attachments may total up to 20 MiB.'))
            return
        }
        if (!isBulkReplyMode && recipientCount === 0) return
        const from = !isBulkReplyMode && !isForwardMode ? (draft?.from || '').trim() || accountEmail : accountEmail
        if (from && !/^[^\s@<>;,]+@[^\s@<>;,]+\.[^\s@<>;,]+$/.test(from)) {
            window.alert(t('Check the sender address.'))
            return
        }
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
            from: from || '',
            scheduledSendAt: scheduled ? scheduled.toISOString() : '',
            requestDeliveryReceipt: requestDeliveryReceipt && !isBulkReplyMode && !isForwardMode,
            requestReadReceipt: requestReceipt && !isBulkReplyMode && !isForwardMode,
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
    }, [accountEmail, accountId, draft, isBulkReplyMode, isForwardMode, onSend, recipientCount, recipients, requestDeliveryReceipt, requestReceipt, scheduleDate, t])

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

    const [attachmentError, setAttachmentError] = useState('')

    const handleFilesSelected = useCallback(async (fileList, disposition) => {
        const files = Array.from(fileList || [])
        if (files.length === 0) return

        const totalBytes = (draft?.attachments || []).reduce((total, attachment) =>
            total + Math.max(attachment.size || 0, (attachment.base64 || '').length * 3 / 4), 0)
        if (files.reduce((total, file) => total + file.size, totalBytes) > ATTACHMENT_LIMIT) {
            setAttachmentError(t('Attachments may total up to 20 MiB.'))
            return
        }
        setAttachmentError('')
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
        })).catch(() => {
            setAttachmentError(t('Could not read the selected attachments.'))
            return null
        })
        if (!prepared) return

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
    }, [draft?.attachments, patchDraft, t])

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
        <div className="compose-view"
            onDragOver={(event) => {
                if (!event.dataTransfer.types.includes('Files')) return
                event.preventDefault()
                event.dataTransfer.dropEffect = 'copy'
            }}
            onDrop={(event) => {
                if (!event.dataTransfer.files.length) return
                event.preventDefault()
                event.stopPropagation()
                void handleFilesSelected(event.dataTransfer.files, 'attachment')
            }}>
            {attachmentError && <div role="alert">{attachmentError}</div>}
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
                <div className="cv-banner__title">{forwardCount === 1 ? t('Forwarding message') : t('Forwarding {{count}} messages', { count: forwardCount })}</div>
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
                    {isBulkReplyMode || isForwardMode
                        ? <div className="cv-from-value">{accountEmail || '—'}</div>
                        : <SenderAliases key={accountId} accountId={accountId} accountEmail={accountEmail}
                            value={draft?.from} onChange={(from) => patchDraft({ from })} />}
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

            {accountId && <ComposeSnippets key={accountId} accountId={accountId} draft={draft} onApply={(entry, kind) => {
                const html = entry.htmlBody || seedHtmlFromPlainText(entry.plainBody || '')
                if (kind === 'templates') {
                    if ((draft?.subject || draft?.plainBody || draft?.htmlBody) &&
                        !window.confirm(t('Replace the subject and message body?'))) return
                    patchDraft({ subject: entry.subject, plainBody: entry.plainBody, htmlBody: html })
                } else {
                    patchDraft({
                        plainBody: [draft?.plainBody, entry.plainBody].filter(Boolean).join('\n\n'),
                        htmlBody: (draft?.htmlBody || '') + html,
                    })
                }
            }} />}

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
                {!linkedEditor && !isBulkReplyMode && !isForwardMode && (
                    <label>
                        <input type="checkbox" checked={requestReceipt} onChange={(event) => setRequestReceipt(event.target.checked)} />
                        {t('Request read receipt')}
                    </label>
                )}
                {!linkedEditor && !isBulkReplyMode && !isForwardMode && (
                    <label>
                        <input type="checkbox" checked={requestDeliveryReceipt}
                            onChange={(event) => setRequestDeliveryReceipt(event.target.checked)} />
                        {t('Request delivery receipt')}
                    </label>
                )}
                {!linkedEditor && !isBulkReplyMode && !isForwardMode && (
                    <label>
                        {t('Schedule send')}
                        {' '}
                        <input type="datetime-local" value={scheduleDate}
                            onChange={(event) => setScheduleDate(event.target.value)} />
                    </label>
                )}
                <button
                    type="button"
                    className="cv-send-btn"
                    onClick={handleSend}
                    disabled={linkedEditor || sending || (!isBulkReplyMode && recipientCount === 0)}
                >
                    {sending ? <><ComposeIcon name="clock" /> {t('Sending...')}</>
                        : isBulkReplyMode
                            ? <><ComposeIcon name="reply" /> {t('Send replies ({{count}})', { count: bulkReplyCount })}</>
                            : isForwardMode
                                ? <><ComposeIcon name="forward" /> {t('Forward')}</>
                                : <><ComposeIcon name="send" /> {t(scheduleDate ? 'Schedule send' : 'Send')}</>}
                </button>
                <button
                    type="button"
                    className="cv-discard-btn"
                    onClick={onDiscard}
                >
                    <ComposeIcon name="discard" /> {t(linkedTab ? 'Close linked tab' : linkedEditor ? 'Close linked window' : 'Discard')}
                </button>
            </div>
        </div>
    )
}
