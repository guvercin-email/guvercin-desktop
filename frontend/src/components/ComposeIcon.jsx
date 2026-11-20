const paths = {
    undo: 'M9 5 4 10l5 5M4 10h10a5 5 0 0 1 0 10h-3',
    redo: 'm15 5 5 5-5 5m5-5H10a5 5 0 0 0 0 10h3',
    bold: 'M7 12h6a4 4 0 0 0 0-8H7v16h7a4 4 0 0 0 0-8h-1',
    italic: 'M11 4h7M6 20h7M14.5 4l-5 16',
    underline: 'M6 4v7a6 6 0 0 0 12 0V4M4 21h16',
    strike: 'M17 7c0-2-2-3-5-3S7 5.5 7 8c0 2 2 3 5 4s5 2 5 4c0 2.5-2 4-5 4s-5-1-5-3M3 12h18',
    textColor: 'm6 15 6-12 6 12M8 11h8',
    highlight: 'm6 12 8-8a2 2 0 0 1 3 0l2 2a2 2 0 0 1 0 3l-8 8H6v-5Zm0 2-2 3h4m3-10 5 5',
    alignLeft: 'M4 5h16M4 10h10M4 15h16M4 20h10',
    alignCenter: 'M4 5h16M7 10h10M4 15h16M7 20h10',
    alignRight: 'M4 5h16m-10 5h10M4 15h16m-10 5h10',
    numberedList: 'M11 7h9m-9 10h9M4 5l2-1v6m-2 0h4M4 15a2 2 0 0 1 3.5 1.3C7.5 18 4 19 4 21h4',
    bulletList: 'M9 7h11M9 12h11M9 17h11M4 7h.01M4 12h.01M4 17h.01',
    outdent: 'M4 5h16m-9 5h9m-9 5h9M4 20h16M7 9l-3 3 3 3',
    indent: 'M4 5h16m-9 5h9m-9 5h9M4 20h16M4 9l3 3-3 3',
    quote: 'M10 7H4v6h6v-6Zm10 0h-6v6h6V7ZM10 13c0 2.5-1 4-4 5m14-5c0 2.5-1 4-4 5',
    link: 'm10 14 4-4m-6 6-1 1a3.5 3.5 0 0 1-5-5l4-4a3.5 3.5 0 0 1 5 0m5 0 1-1a3.5 3.5 0 0 1 5 5l-4 4a3.5 3.5 0 0 1-5 0',
    clear: 'm4 12 8-8a2 2 0 0 1 3 0l5 5a2 2 0 0 1 0 3l-8 8H8l-4-4a3 3 0 0 1 0-4Zm3-3 9 8m-4 3h8',
    changeCase: 'm2 17 4-10 4 10m-6.5-3.5h5m12 3.5v-7m0 2a3 3 0 1 0 0 3',
    lineSpacing: 'M7 4v16M4 7l3-3 3 3m-6 10 3 3 3-3m4-12h6m-6 7h6m-6 7h6',
    subscript: 'm4 7 8 10m0-10L4 17m11-3a2 2 0 1 1 3 2c-1.5 1-3 2-3 4h5',
    superscript: 'm4 7 8 10m0-10L4 17m11-13a2 2 0 1 1 3 2c-1.5 1-3 2-3 4h5',
    image: 'M6 4h12a2 2 0 0 1 2 2v12a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2Zm-2 12 5-5 4 4 3-3 4 4M15 8h.01',
    emoji: 'M21 12a9 9 0 1 1-18 0 9 9 0 1 1 18 0ZM8 14a4.5 4.5 0 0 0 8 0M9 9h.01M15 9h.01',
    more: 'M5 12h.01m7 0h.01m7 0h.01',
    attachment: 'm9 13 6-6a2 2 0 0 1 3 3l-8 8a3.5 3.5 0 0 1-5-5l9-9a5 5 0 0 1 7 7l-9 9',
    send: 'm21 3-7 18-3.5-7.5L3 10 21 3ZM10.5 13.5 21 3',
    reply: 'M10 6l-6 6 6 6m-6-6h10a6 6 0 0 1 6 6',
    forward: 'm14 6 6 6-6 6m6-6H10a6 6 0 0 0-6 6',
    openTab: 'M10 5H5a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5M3 10h7m4-7h7v7m0-7-8 8',
    openWindow: 'M10 5H5a2 2 0 0 0-2 2v12a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-5m-5-11h7v7m0-7-8 8',
    close: 'm6 6 12 12M18 6 6 18',
    discard: 'M4 6h16m-11 0V4h6v2M6 6l1 14h10l1-14m-8 4v6m4-6v6',
    clock: 'M21 12a9 9 0 1 1-18 0 9 9 0 1 1 18 0Zm-9-5v5l3 2',
}

export default function ComposeIcon({ name, size = 16, swatch }) {
    return (
        <svg
            width={size}
            height={size}
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.8"
            strokeLinecap="round"
            strokeLinejoin="round"
            aria-hidden="true"
            focusable="false"
        >
            <path d={paths[name]} />
            {swatch && <rect x="4" y="19" width="16" height="4" rx="1" fill={swatch} strokeOpacity=".4" strokeWidth="1" />}
        </svg>
    )
}
