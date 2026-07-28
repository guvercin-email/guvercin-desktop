import test from 'node:test'
import assert from 'node:assert/strict'

const { parseAttachmentUri } = await import('./attachmentInbox.js')

test('parseAttachmentUri reads a single selected file', () => {
  assert.deepEqual(
    parseAttachmentUri('guvercin://attach-file?path=%2Ftmp%2Freport.pdf'),
    ['/tmp/report.pdf'],
  )
})

test('parseAttachmentUri reads a whole multi-file selection', () => {
  assert.deepEqual(
    parseAttachmentUri('guvercin://attach-file?path=%2Ftmp%2Fa.pdf&path=%2Ftmp%2Fb%20c.png'),
    ['/tmp/a.pdf', '/tmp/b c.png'],
  )
})

test('parseAttachmentUri keeps non-ASCII names intact', () => {
  // The file managers encode byte by byte, so UTF-8 survives the round trip.
  assert.deepEqual(
    parseAttachmentUri('guvercin://attach-file?path=%2Ftmp%2Fg%C3%BCvercin.eml'),
    ['/tmp/güvercin.eml'],
  )
})

test('parseAttachmentUri ignores anything else', () => {
  assert.deepEqual(parseAttachmentUri('mailto:someone@example.com'), [])
  assert.deepEqual(parseAttachmentUri('guvercin://attach-file?'), [])
  assert.deepEqual(parseAttachmentUri('guvercin://attach-file?path='), [])
  assert.deepEqual(parseAttachmentUri(null), [])
})
