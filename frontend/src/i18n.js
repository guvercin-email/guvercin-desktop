import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
// Use Vite's glob import to load all translation JSON files dynamically
const modules = import.meta.glob('./locales/**/*.json', { eager: true });
const resources = {};

for (const path in modules) {
    const parts = path.split('/');
    if (parts.length >= 3) {
        const lang = parts[2];
        resources[lang] = {
            translation: modules[path].default || modules[path]
        };
    }
}

// Languages written right-to-left. Keyed by base language so regional variants
// (ar-bh, ar-ps) are covered without listing each one.
const RTL_LANGUAGES = new Set(['ar', 'fa', 'ps', 'ur', 'he', 'yi', 'dv', 'ku', 'sd', 'ug']);

export function isRtlLanguage(lang) {
    if (!lang) return false;
    return RTL_LANGUAGES.has(String(lang).toLowerCase().split(/[-_]/)[0]);
}

// Mirror the active language onto <html lang/dir> so the whole document — including
// text selection, caret movement and the CSS logical properties in our stylesheets —
// lays out in the right direction.
export function applyDocumentDirection(lang) {
    if (typeof document === 'undefined') return;
    const root = document.documentElement;
    root.lang = lang || 'en';
    root.dir = isRtlLanguage(lang) ? 'rtl' : 'ltr';
}

function getInitialLanguage() {
    const saved = localStorage.getItem('temp_language') || localStorage.getItem('language');
    if (saved) return saved;

    try {
        const sysLang = navigator.language || navigator.languages?.[0];
        if (sysLang) {
            const normalized = sysLang.toLowerCase();
            if (normalized === 'tr' || normalized.startsWith('tr-')) {
                return 'tr';
            }
        }
    } catch (e) {
        console.error('Failed to detect system language', e);
    }
    return 'en';
}

const initialLang = getInitialLanguage();
// Ensure the initial language is saved so other components can reference it correctly
if (!localStorage.getItem('temp_language') && !localStorage.getItem('language')) {
    localStorage.setItem('temp_language', initialLang);
}

i18n
    .use(initReactI18next)
    .init({
        resources,
        lng: initialLang,
        fallbackLng: "en",
        interpolation: {
            escapeValue: false
        }
    });

applyDocumentDirection(initialLang);
i18n.on('languageChanged', applyDocumentDirection);

export default i18n;

// Export available languages for UI
export function getAvailableLanguages() {
    return Object.keys(resources).sort();
}
