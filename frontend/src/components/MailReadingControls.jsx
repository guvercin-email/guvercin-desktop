import { useState } from 'react'
import { useTranslation } from 'react-i18next'

const ZOOM_LEVELS = [75, 90, 100, 110, 125, 150, 175, 200]
const FONT_SIZES = [12, 14, 15, 16, 18, 20, 24]

export function useMailReadingSettings() {
    const [zoom, setZoom] = useState(() => {
        const value = Number(localStorage.getItem('mail_reading_zoom'))
        return ZOOM_LEVELS.includes(value) ? value : 100
    })
    const [fontSize, setFontSize] = useState(() => {
        const value = Number(localStorage.getItem('mail_reading_font_size'))
        return FONT_SIZES.includes(value) ? value : 15
    })
    return {
        zoom,
        fontSize,
        style: {
            '--mail-reading-zoom': zoom / 100,
            '--mail-reading-font-size': `${fontSize}px`,
        },
        changeZoom(value) {
            localStorage.setItem('mail_reading_zoom', String(value))
            setZoom(value)
        },
        changeFontSize(value) {
            localStorage.setItem('mail_reading_font_size', String(value))
            setFontSize(value)
        },
    }
}

export default function MailReadingControls({ settings }) {
    const { t } = useTranslation()
    return (
        <div className="db-mail-reading-controls">
            <label>
                {t('Message zoom')}
                <select value={settings.zoom} onChange={(event) => settings.changeZoom(Number(event.target.value))}>
                    {ZOOM_LEVELS.map((value) => <option key={value} value={value}>{value}%</option>)}
                </select>
            </label>
            <label>
                {t('Plain-text size')}
                <select value={settings.fontSize} onChange={(event) => settings.changeFontSize(Number(event.target.value))}>
                    {FONT_SIZES.map((value) => <option key={value} value={value}>{value} px</option>)}
                </select>
            </label>
        </div>
    )
}
