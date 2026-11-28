import { useEffect, useState } from 'react'
import { useTranslation } from 'react-i18next'
import { canPreviewOffice, readOfficePreview } from '../utils/officePreview.js'

const MAX_BYTES = 20 * 1024 * 1024

async function attachmentBytes(base64, href, signal) {
    if (base64) {
        if (base64.length > Math.ceil(MAX_BYTES / 3) * 4) throw new Error('Attachment is too large')
        return Uint8Array.from(atob(base64), (value) => value.charCodeAt(0))
    }
    const response = await fetch(href, { signal })
    if (!response.ok || Number(response.headers.get('content-length')) > MAX_BYTES) throw new Error('Attachment unavailable')
    const reader = response.body.getReader()
    const chunks = []
    let size = 0
    try {
        while (true) {
            const { done, value } = await reader.read()
            if (done) break
            size += value.length
            if (size > MAX_BYTES) throw new Error('Attachment is too large')
            chunks.push(value)
        }
    } finally {
        await reader.cancel().catch(() => {})
    }
    const bytes = new Uint8Array(size)
    let offset = 0
    for (const chunk of chunks) {
        bytes.set(chunk, offset)
        offset += chunk.length
    }
    return bytes
}

export function OfficeAttachmentContents({ name, base64, href }) {
    const { t } = useTranslation()
    const [result, setResult] = useState(null)
    useEffect(() => {
        const controller = new AbortController()
        attachmentBytes(base64, href, controller.signal)
            .then((bytes) => readOfficePreview(bytes, name))
            .then((sections) => { if (!controller.signal.aborted) setResult({ name, base64, href, sections }) })
            .catch(() => { if (!controller.signal.aborted) setResult({ name, base64, href, sections: [] }) })
        return () => controller.abort()
    }, [name, base64, href])

    if (!result || result.name !== name || result.base64 !== base64 || result.href !== href) {
        return <span role="status">{t('Loading preview…')}</span>
    }
    if (!result.sections.some(({ text, rows }) => text || rows.length)) {
        return <span role="status">{t('Preview unavailable.')}</span>
    }
    return (
        <div className="cv-attachments__preview-document office-attachment-preview">
            {result.sections.map(({ title, text, rows }, index) => (
                <section key={index} aria-label={title || t('Section {{number}}', { number: index + 1 })}>
                    {title && <h4>{title}</h4>}
                    {text && <pre>{text}</pre>}
                    {rows.length > 0 && <table><tbody>
                        {rows.map((row, i) => <tr key={i}>{row.map((cell, j) => <td key={j}>{cell}</td>)}</tr>)}
                    </tbody></table>}
                </section>
            ))}
        </div>
    )
}

export default function OfficeAttachmentPreview({ name, base64, href }) {
    const { t } = useTranslation()
    const [open, setOpen] = useState(false)
    if (!canPreviewOffice(name) || (!base64 && !href)) return null
    return (
        <details onToggle={(event) => setOpen(event.currentTarget.open)}>
            <summary className="db-attachments__link">{t('Preview')}</summary>
            {open && <OfficeAttachmentContents name={name} base64={base64} href={href} />}
        </details>
    )
}
