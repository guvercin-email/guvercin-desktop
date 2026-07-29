import i18n from 'i18next';
import { initReactI18next } from 'react-i18next';
// English is the fallback, so it is the one bundle that always has to be present.
import enTranslation from './locales/en/translation.json';

// The other 63 locales are ~1.7 MB of JSON in total and a user only ever reads
// one of them, so they stay behind per-language dynamic imports. `import.meta.glob`
// without `eager` gives us a { path: () => import(path) } map that Vite turns into
// one chunk per locale.
// English is excluded: it is statically imported above, and letting it match here
// too would leave rollup with the same module reachable both ways.
const localeLoaders = import.meta.glob(['./locales/**/*.json', '!./locales/en/**']);

const LOCALE_BY_LANG = { en: null };
for (const path in localeLoaders) {
    const parts = path.split('/');
    if (parts.length >= 3) LOCALE_BY_LANG[parts[2]] = localeLoaders[path];
}

/**
 * Makes sure `lang`'s bundle is registered with i18next. Safe to call repeatedly;
 * resolves immediately once a language has been loaded. Unknown languages resolve
 * without doing anything — i18next then falls back to English.
 */
export async function loadLanguage(lang) {
    if (!lang || lang === 'en') return;
    if (i18n.hasResourceBundle(lang, 'translation')) return;
    const loader = LOCALE_BY_LANG[lang];
    if (!loader) return;
    try {
        const mod = await loader();
        i18n.addResourceBundle(lang, 'translation', mod.default || mod, true, true);
    } catch (error) {
        console.error(`Failed to load the ${lang} translation`, error);
    }
}

/** Loads `lang` before switching, so the UI never flashes the fallback text. */
export async function changeLanguage(lang) {
    await loadLanguage(lang);
    return i18n.changeLanguage(lang);
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
        resources: { en: { translation: enTranslation } },
        lng: initialLang,
        fallbackLng: "en",
        interpolation: {
            escapeValue: false
        }
    });

applyDocumentDirection(initialLang);
i18n.on('languageChanged', applyDocumentDirection);

/**
 * Resolves once the startup language is usable. `main.jsx` awaits it before the
 * first render so a non-English user never sees a frame of English.
 */
export const i18nReady = loadLanguage(initialLang);

export default i18n;

/** Every language shipped, whether or not its bundle has been loaded yet. */
export function getAvailableLanguages() {
    return Object.keys(LOCALE_BY_LANG).sort();
}
