import test from 'node:test'
import assert from 'node:assert/strict'
import { deflateRawSync } from 'node:zlib'
import { JSDOM } from 'jsdom'
import { readOfficePreview } from './officePreview.js'

globalThis.DOMParser = new JSDOM('').window.DOMParser

function zipPart(path, source, declaredSize = Buffer.byteLength(source)) {
    const name = Buffer.from(path)
    const data = deflateRawSync(source)
    const local = Buffer.alloc(30)
    local.writeUInt32LE(0x04034b50)
    local.writeUInt16LE(8, 8)
    local.writeUInt16LE(name.length, 26)
    const directory = Buffer.alloc(46)
    directory.writeUInt32LE(0x02014b50)
    directory.writeUInt16LE(8, 10)
    directory.writeUInt32LE(data.length, 20)
    directory.writeUInt32LE(declaredSize, 24)
    directory.writeUInt16LE(name.length, 28)
    const end = Buffer.alloc(22)
    end.writeUInt32LE(0x06054b50)
    end.writeUInt16LE(1, 8)
    end.writeUInt16LE(1, 10)
    end.writeUInt32LE(directory.length + name.length, 12)
    end.writeUInt32LE(local.length + name.length + data.length, 16)
    return Buffer.concat([local, name, data, directory, name, end])
}

test('Office preview reads selected XML from a deflated attachment', async () => {
    const bytes = zipPart('word/document.xml', '<document><p><t>Hello &amp; goodbye</t></p></document>')
    assert.deepEqual(await readOfficePreview(bytes, 'sample.DOCX'), [{ title: '', text: 'Hello & goodbye', rows: [] }])
})

test('Office preview rejects malformed metadata and XML declarations', async () => {
    const xml = '<document><p><t>Document</t></p></document>'
    await assert.rejects(readOfficePreview(zipPart('word/document.xml', xml, 1), 'sample.docx'), /output size/)
    await assert.rejects(readOfficePreview(zipPart('word/document.xml', '<!DOCTYPE x><x/>'), 'sample.docx'), /XML declaration/)
})
