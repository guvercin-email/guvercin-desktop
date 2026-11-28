const MAX_ARCHIVE_BYTES = 20 * 1024 * 1024
const MAX_XML_BYTES = 2 * 1024 * 1024
const MAX_TOTAL_XML_BYTES = 8 * 1024 * 1024
const OFFICE_EXTENSIONS = new Set(['docx', 'xlsx', 'pptx', 'odt', 'ods', 'odp'])
const decoder = new TextDecoder()
const fileExtension = (name = '') => name.toLowerCase().split('.').at(-1)

export const canPreviewOffice = (name) => OFFICE_EXTENSIONS.has(fileExtension(name))

function centralDirectory(bytes) {
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength)
    const u16 = (offset) => view.getUint16(offset, true)
    const u32 = (offset) => view.getUint32(offset, true)
    let end = bytes.length - 22
    const lower = Math.max(0, end - 65535)
    while (end >= lower && (u32(end) !== 0x06054b50 || end + 22 + u16(end + 20) !== bytes.length)) end--
    if (end < lower || u16(end + 4) || u16(end + 6) || u16(end + 8) !== u16(end + 10)) {
        throw new Error('Invalid ZIP directory')
    }
    const count = u16(end + 10)
    let offset = u32(end + 16)
    const directoryStart = offset
    const directoryEnd = offset + u32(end + 12)
    if (count > 10000 || directoryEnd > end) throw new Error('Invalid ZIP directory')
    const entries = new Map()
    for (let index = 0; index < count; index++) {
        if (offset + 46 > directoryEnd || u32(offset) !== 0x02014b50) throw new Error('Invalid ZIP entry')
        const nameLength = u16(offset + 28)
        const nameEnd = offset + 46 + nameLength
        if (nameEnd > directoryEnd) throw new Error('Invalid ZIP entry name')
        const name = decoder.decode(bytes.subarray(offset + 46, nameEnd))
        entries.set(name, {
            flag: u16(offset + 8), method: u16(offset + 10), compressed: u32(offset + 20),
            size: u32(offset + 24), start: u32(offset + 42),
        })
        offset = nameEnd + u16(offset + 30) + u16(offset + 32)
    }
    if (offset !== directoryEnd) throw new Error('Invalid ZIP directory size')
    return { entries, directoryStart, u16, u32 }
}

async function inflate(input, declaredSize) {
    const reader = new Blob([input]).stream().pipeThrough(new DecompressionStream('deflate-raw')).getReader()
    const output = new Uint8Array(declaredSize)
    let offset = 0
    try {
        while (true) {
            const { done, value } = await reader.read()
            if (done) break
            if (offset + value.length > declaredSize) throw new Error('Invalid ZIP output size')
            output.set(value, offset)
            offset += value.length
        }
    } finally {
        await reader.cancel().catch(() => {})
    }
    if (offset !== declaredSize) throw new Error('Incomplete ZIP output')
    return output
}

const elements = (element, name) => Array.from(element.getElementsByTagNameNS('*', name))
const isOutsideTable = (element) => {
    for (let parent = element.parentElement; parent; parent = parent.parentElement) {
        if (parent.localName === 'table') return false
    }
    return true
}
const runText = (element) => elements(element, 't').map((run) => run.textContent).join('')
const tidy = (lines) => lines.map((line) => line.trimEnd()).filter((line) => line.trim()).join('\n')

export async function readOfficePreview(bytes, name) {
    if (bytes.length > MAX_ARCHIVE_BYTES) throw new Error('Attachment is too large to preview')
    const { entries, directoryStart, u16, u32 } = centralDirectory(bytes)
    let remaining = MAX_TOTAL_XML_BYTES
    const xml = async (path) => {
        const entry = entries.get(path)
        if (!entry) throw new Error('Office document part is missing')
        const { flag, method, compressed, size, start } = entry
        if (flag & 1 || size > MAX_XML_BYTES || size > remaining || start + 30 > directoryStart
            || u32(start) !== 0x04034b50 || ![0, 8].includes(method)) {
            throw new Error('Unsupported Office document part')
        }
        const offset = start + 30 + u16(start + 26) + u16(start + 28)
        if (offset + compressed > directoryStart || (method === 0 && size !== compressed)) {
            throw new Error('Invalid ZIP data')
        }
        remaining -= size
        const input = bytes.subarray(offset, offset + compressed)
        const source = decoder.decode(method === 0 ? input : await inflate(input, size))
        if (/<!DOCTYPE|<!ENTITY/i.test(source)) throw new Error('Unsupported XML declaration')
        const document = new DOMParser().parseFromString(source, 'application/xml')
        if (elements(document, 'parsererror').length) throw new Error('Invalid Office document XML')
        return document
    }

    const extension = fileExtension(name)
    if (extension === 'docx') {
        const text = tidy(elements(await xml('word/document.xml'), 'p').map(runText))
        return [{ title: '', text, rows: [] }]
    }
    if (extension === 'pptx') {
        const slides = [...entries.keys()].filter((path) => /^ppt\/slides\/slide\d+\.xml$/.test(path))
            .sort((a, b) => a.localeCompare(b, undefined, { numeric: true })).slice(0, 20)
        const previews = []
        for (const path of slides) {
            previews.push({ title: '', text: tidy(elements(await xml(path), 'p').map(runText)), rows: [] })
        }
        return previews
    }
    if (extension === 'xlsx') {
        const shared = entries.has('xl/sharedStrings.xml') ? elements(await xml('xl/sharedStrings.xml'), 'si').map(runText) : []
        const sheets = [...entries.keys()].filter((path) => /^xl\/worksheets\/sheet\d+\.xml$/.test(path))
            .sort((a, b) => a.localeCompare(b, undefined, { numeric: true })).slice(0, 8)
        const previews = []
        for (const path of sheets) {
            const rows = elements(await xml(path), 'row').slice(0, 60).map((row) => {
                const cells = []
                for (const cell of elements(row, 'c')) {
                    const column = (cell.getAttribute('r') || '').match(/^[A-Z]+/)?.[0]
                    const index = column ? [...column].reduce((sum, letter) => sum * 26 + letter.charCodeAt(0) - 64, 0) - 1 : cells.length
                    if (index > 19) continue
                    while (cells.length <= index) cells.push('')
                    const value = elements(cell, 'v')[0]?.textContent || ''
                    cells[index] = cell.getAttribute('t') === 's' ? shared[Number(value)] || '' : runText(cell) || value
                }
                return cells
            })
            previews.push({ title: '', text: '', rows })
        }
        return previews
    }
    const document = await xml('content.xml')
    if (extension === 'ods') {
        return elements(document, 'table').slice(0, 8).map((table) => ({
            title: table.getAttribute('table:name') || '', text: '',
            rows: elements(table, 'table-row').slice(0, 60).map((row) =>
                elements(row, 'table-cell').slice(0, 20).map((cell) => cell.textContent.trim())),
        }))
    }
    if (extension === 'odp') {
        return elements(document, 'page').slice(0, 20).map((page) => ({
            title: page.getAttribute('draw:name') || '', text: tidy(elements(page, 'p').map((node) => node.textContent)), rows: [],
        }))
    }
    return [{ title: '', text: tidy(elements(document, 'p').filter(isOutsideTable).map((node) => node.textContent)), rows: [] }]
}
