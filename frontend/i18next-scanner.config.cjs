const fs = require('fs');
const path = require('path');

// Every locale that ships, read off disk so adding a directory under src/locales
// is the only step needed — the list used to be hard-coded to en/tr, which meant
// a scan rewrote two files and silently ignored the other 62.
const LOCALES = fs
    .readdirSync(path.join(__dirname, 'src/locales'), { withFileTypes: true })
    .filter((entry) => entry.isDirectory())
    .map((entry) => entry.name)
    .sort();

module.exports = {
    input: [
        'src/**/*.{js,jsx}',
    ],
    output: './',
    options: {
        debug: true,
        // Deliberately off. A handful of keys are looked up through a variable
        // — t(theme.labelKey), the task priorities, the login candidate labels —
        // and the scanner cannot see those, so pruning on its word would delete
        // live translations. Unused keys have to be confirmed by hand.
        removeUnusedKeys: false,
        sort: true,
        func: {
            list: ['t'],
            extensions: ['.js', '.jsx'],
        },
        lngs: LOCALES,
        defaultLng: 'en',
        resource: {
            loadPath: 'src/locales/{{lng}}/{{ns}}.json',
            savePath: 'src/locales/{{lng}}/{{ns}}.json',
            jsonIndent: 2,
            lineEnding: '\n',
        },
        nsSeparator: false,
        keySeparator: false,
        interpolation: {
            prefix: '{{',
            suffix: '}}',
        },
    },
};
