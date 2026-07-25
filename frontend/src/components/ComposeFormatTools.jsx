import { Fragment, useEffect, useLayoutEffect, useRef, useState } from 'react'
import { useComposeEditorContext } from '../context/ComposeEditorContext.jsx'
import './ComposeFormatTools.css'

const FONT_FAMILIES = [
    { value: 'sans-serif', label: 'Sans Serif' },
    { value: 'Arial, sans-serif', label: 'Arial' },
    { value: "'Aptos', sans-serif", label: 'Aptos' },
    { value: "'Georgia', serif", label: 'Georgia' },
    { value: "'Times New Roman', serif", label: 'Times New Roman' },
    { value: "'Courier New', monospace", label: 'Courier New' },
    { value: "'Verdana', sans-serif", label: 'Verdana' },
    { value: "'Trebuchet MS', sans-serif", label: 'Trebuchet MS' },
]

const FONT_SIZES = ['10px','11px','12px','13px','14px','16px','18px','20px','24px','28px','32px','36px']
const LINE_SPACINGS = ['1.0', '1.15', '1.4', '1.6', '2.0', '2.5', '3.0']

/* ── Inline monochrome icons (no emoji) ── */
const ico = { width: 16, height: 16, viewBox: '0 0 16 16', fill: 'none', stroke: 'currentColor', strokeWidth: 1.5, strokeLinecap: 'round', strokeLinejoin: 'round' }
const HighlightIcon = () => (
    <svg {...ico}><path d="M4 11l5-5 3 3-5 5H4v-3z" /><path d="M9 6l1.5-1.5a1.5 1.5 0 0 1 2 2L11 8" /><path d="M3 14h10" strokeWidth="2" /></svg>
)
const LinkIcon = () => (
    <svg {...ico}><path d="M6.5 9.5l3-3" /><path d="M7 4.5l1-1a2.5 2.5 0 0 1 3.5 3.5l-1 1" /><path d="M9 11.5l-1 1A2.5 2.5 0 0 1 4.5 9l1-1" /></svg>
)
const EraserIcon = () => (
    <svg {...ico}><path d="M4 12l6-6a1.5 1.5 0 0 1 2 0l1 1a1.5 1.5 0 0 1 0 2l-5 5H6l-2-2z" /><path d="M3 14h8" /></svg>
)
const ImageIcon = () => (
    <svg {...ico}><rect x="2.5" y="3.5" width="11" height="9" rx="1.5" /><circle cx="6" cy="7" r="1" /><path d="M3.5 12l3-3 2 2 2-2 2 2.5" /></svg>
)

/**
 * The mail formatting toolbar, rendered outside the editor (in the app ribbon
 * or a detached compose window). All commands are proxied to the currently
 * mounted ComposeEditor via ComposeEditorContext.
 *
 * Like the ribbon's SubmenuBar, it lays the tools out in a single row and moves
 * whatever doesn't fit into a "⋯" overflow menu on the right — no wrapping.
 */
export default function ComposeFormatTools() {
    const ctx = useComposeEditorContext()
    const controller = ctx?.controller || null
    const snapshot = ctx?.snapshot || null
    const disabled = !controller

    // Popups are position:fixed so they never get clipped by the ribbon row.
    const [linkPos, setLinkPos] = useState(null)
    const [spacingPos, setSpacingPos] = useState(null)
    const [linkUrl, setLinkUrl] = useState('')
    const [linkTitle, setLinkTitle] = useState('')
    const [moreOpen, setMoreOpen] = useState(false)

    const moreRef = useRef(null)

    useEffect(() => {
        const handleDown = (e) => {
            if (e.target.closest('.cf-popup') || e.target.closest('.cf-popup-trigger')) return
            setLinkPos(null)
            setSpacingPos(null)
            if (moreRef.current && !moreRef.current.contains(e.target)) setMoreOpen(false)
        }
        document.addEventListener('mousedown', handleDown)
        return () => document.removeEventListener('mousedown', handleDown)
    }, [])

    const active = snapshot?.active || {}
    const fontFamily = snapshot?.fontFamily || 'sans-serif'
    const fontSize = snapshot?.fontSize || '14px'
    const textColor = snapshot?.textColor || '#000000'
    const highlight = snapshot?.highlight || '#ffff00'
    const lineSpacing = snapshot?.lineSpacing || '1.6'

    const run = (fn) => {
        if (disabled) return
        fn()
    }

    const anchorFor = (e) => {
        // Read the rect now — e.currentTarget is nulled once the handler returns.
        const r = e.currentTarget.getBoundingClientRect()
        return { top: r.bottom + 4, left: r.left }
    }

    const openLink = (e) => {
        if (disabled) return
        if (controller.isLinkActive()) {
            controller.removeLink()
            return
        }
        const pos = anchorFor(e)
        setSpacingPos(null)
        setLinkPos((prev) => (prev ? null : pos))
    }

    const openSpacing = (e) => {
        if (disabled) return
        const pos = anchorFor(e)
        setLinkPos(null)
        setSpacingPos((prev) => (prev ? null : pos))
    }

    const handleInsertLink = () => {
        if (!controller || !linkUrl.trim()) return
        controller.insertLink(linkUrl, linkTitle)
        setLinkPos(null)
        setLinkUrl('')
        setLinkTitle('')
    }

    // ── Tool groups (each stays intact; overflow moves whole groups) ──
    const groups = [
        <div className="ce-toolbar-group" key="history">
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.undo())} title="Geri Al">↶</button>
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.redo())} title="Yinele">↷</button>
        </div>,
        <div className="ce-toolbar-group" key="font">
            <select value={fontFamily} disabled={disabled} onChange={(e) => run(() => controller.setFontFamily(e.target.value))} title="Yazı Tipi">
                {FONT_FAMILIES.map((f) => <option key={f.value} value={f.value}>{f.label}</option>)}
            </select>
        </div>,
        <div className="ce-toolbar-group" key="size">
            <select value={fontSize} disabled={disabled} onChange={(e) => run(() => controller.setFontSize(e.target.value))} title="Yazı Boyutu">
                {FONT_SIZES.map((s) => <option key={s} value={s}>{parseInt(s, 10)}</option>)}
            </select>
        </div>,
        <div className="ce-toolbar-group" key="marks">
            <button type="button" disabled={disabled} className={active.strong ? 'active' : ''} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.toggleBold())} title="Kalın"><b>B</b></button>
            <button type="button" disabled={disabled} className={active.em ? 'active' : ''} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.toggleItalic())} title="İtalik"><i>I</i></button>
            <button type="button" disabled={disabled} className={active.underline ? 'active' : ''} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.toggleUnderline())} title="Altı Çizili"><u>U</u></button>
            <button type="button" disabled={disabled} className={active.strikethrough ? 'active' : ''} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.toggleStrike())} title="Üstü Çizili"><s>S</s></button>
        </div>,
        <div className="ce-toolbar-group" key="color">
            <div className="ce-color-wrap">
                <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} title="Metin Rengi" style={{ color: textColor }}>A</button>
                <input type="color" value={textColor} disabled={disabled} onChange={(e) => run(() => controller.setTextColor(e.target.value))} title="Metin Rengi" />
            </div>
            <div className="ce-color-wrap">
                <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} title="Vurgu Rengi" style={{ color: highlight }}><HighlightIcon /></button>
                <input type="color" value={highlight} disabled={disabled} onChange={(e) => run(() => controller.setHighlight(e.target.value))} title="Vurgu Rengi" />
            </div>
        </div>,
        <div className="ce-toolbar-group" key="align">
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.align('left'))} title="Sola Hizala">⯇</button>
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.align('center'))} title="Ortala">≡</button>
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.align('right'))} title="Sağa Hizala">⯈</button>
        </div>,
        <div className="ce-toolbar-group" key="lists">
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.orderedList())} title="Numaralı Liste">1.</button>
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.bulletList())} title="Madde İşaretli Liste">•</button>
        </div>,
        <div className="ce-toolbar-group" key="indent">
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.outdent())} title="Girintiyi Azalt">⇤</button>
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.indent())} title="Girintiyi Artır">⇥</button>
        </div>,
        <div className="ce-toolbar-group" key="quote">
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.blockquote())} title="Alıntı">❝</button>
        </div>,
        <div className="ce-toolbar-group" key="link">
            <button type="button" disabled={disabled} className={`cf-popup-trigger ${active.link ? 'active' : ''}`} onMouseDown={(e) => e.preventDefault()} onClick={openLink} title="Bağlantı Ekle"><LinkIcon /></button>
        </div>,
        <div className="ce-toolbar-group" key="clear">
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.clearFormatting())} title="Biçimlendirmeyi Temizle"><EraserIcon /></button>
        </div>,
        <div className="ce-toolbar-group" key="case">
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.changeCase())} title="Büyük/Küçük Harf">aA</button>
        </div>,
        <div className="ce-toolbar-group" key="spacing">
            <button type="button" disabled={disabled} className="cf-popup-trigger" onMouseDown={(e) => e.preventDefault()} onClick={openSpacing} title="Satır Aralığı">↕</button>
        </div>,
        <div className="ce-toolbar-group" key="script">
            <button type="button" disabled={disabled} className={active.subscript ? 'active' : ''} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.toggleSubscript())} title="Alt Simge">x₂</button>
            <button type="button" disabled={disabled} className={active.superscript ? 'active' : ''} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.toggleSuperscript())} title="Üst Simge">x²</button>
        </div>,
        <div className="ce-toolbar-group" key="image">
            <button type="button" disabled={disabled} onMouseDown={(e) => e.preventDefault()} onClick={() => run(() => controller.insertImage())} title="Görsel Ekle"><ImageIcon /></button>
        </div>,
    ]

    // ── Overflow measurement: keep one row, push extras into a ⋯ menu ──
    const containerRef = useRef(null)
    const measureRef = useRef(null)
    const [visibleCount, setVisibleCount] = useState(groups.length)

    useLayoutEffect(() => {
        const recompute = () => {
            const container = containerRef.current
            const measure = measureRef.current
            if (!container || !measure) return
            const items = Array.from(measure.querySelectorAll('.ce-toolbar-group'))
            const containerWidth = container.clientWidth
            const widths = items.map((el) => el.getBoundingClientRect().width + 8)
            const total = widths.reduce((a, b) => a + b, 0)
            if (total <= containerWidth) {
                setVisibleCount((prev) => (prev === items.length ? prev : items.length))
                return
            }
            const moreBtnWidth = 44
            let used = moreBtnWidth
            let count = 0
            for (const w of widths) {
                if (used + w <= containerWidth) { used += w; count += 1 } else break
            }
            setVisibleCount((prev) => (prev === count ? prev : count))
        }
        recompute()
        const ro = new ResizeObserver(recompute)
        if (containerRef.current) ro.observe(containerRef.current)
        return () => ro.disconnect()
        // groups is rebuilt each render but its length/structure is stable
        // eslint-disable-next-line react-hooks/exhaustive-deps
    }, [])

    const visible = groups.slice(0, visibleCount)
    const overflow = groups.slice(visibleCount)

    return (
        <div className={`cf-tools-bar ${disabled ? 'cf-tools--disabled' : ''}`} ref={containerRef}>
            {/* Hidden measurer: full set, used only to compute widths */}
            <div className="cf-measure" aria-hidden="true" ref={measureRef}>
                <div className="ce-toolbar cf-tools">{groups.map((g, i) => <Fragment key={i}>{g}</Fragment>)}</div>
            </div>

            {/* Visible single row */}
            <div className="ce-toolbar cf-tools cf-tools--visible">{visible}</div>

            {/* Overflow ⋯ menu */}
            {overflow.length > 0 && (
                <div className="cf-more" ref={moreRef}>
                    <button
                        type="button"
                        className="db-submenu-more-btn cf-popup-trigger"
                        onMouseDown={(e) => e.preventDefault()}
                        onClick={() => setMoreOpen((v) => !v)}
                        title="Daha Fazla"
                    >
                        <img src="/img/icons/three-point.svg" className="svg-icon-inline" />
                    </button>
                    {moreOpen && (
                        <div className="db-overflow-menu cf-overflow-menu">
                            {overflow.map((g, i) => <div key={i} className="cf-overflow-row">{g}</div>)}
                        </div>
                    )}
                </div>
            )}

            {/* Link dialog (fixed, anchored to trigger) */}
            {linkPos && (
                <div className="ce-link-dialog cf-popup" style={{ top: linkPos.top, left: linkPos.left }}>
                    <input type="url" placeholder="https://example.com" value={linkUrl} autoFocus
                        onChange={(e) => setLinkUrl(e.target.value)}
                        onKeyDown={(e) => e.key === 'Enter' && handleInsertLink()} />
                    <input type="text" placeholder="Link başlığı (opsiyonel)" value={linkTitle}
                        onChange={(e) => setLinkTitle(e.target.value)}
                        onKeyDown={(e) => e.key === 'Enter' && handleInsertLink()} />
                    <div className="ce-link-dialog-actions">
                        <button type="button" onClick={() => setLinkPos(null)}>İptal</button>
                        <button type="button" className="primary" onClick={handleInsertLink}>Ekle</button>
                    </div>
                </div>
            )}

            {/* Line-spacing popup (fixed, anchored to trigger) */}
            {spacingPos && (
                <div className="ce-linespacing-popup cf-popup" style={{ top: spacingPos.top, left: spacingPos.left }}>
                    {LINE_SPACINGS.map((s) => (
                        <button key={s} type="button" className={lineSpacing === s ? 'active' : ''}
                            onClick={() => { controller.setLineSpacing(s); setSpacingPos(null) }}>{s}</button>
                    ))}
                </div>
            )}
        </div>
    )
}
