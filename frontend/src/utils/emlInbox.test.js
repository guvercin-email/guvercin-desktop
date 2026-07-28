import test from 'node:test'
import assert from 'node:assert/strict'

const { toEmlPath } = await import('./emlInbox.js')

test('toEmlPath decodes the file:// URLs macOS delivers', () => {
  assert.equal(toEmlPath('file:///Users/me/Mail/note.eml'), '/Users/me/Mail/note.eml')
  assert.equal(toEmlPath('file://localhost/Users/me/a%20note.eml'), '/Users/me/a note.eml')
})

test('toEmlPath keeps the bare paths Windows and Linux pass as arguments', () => {
  assert.equal(toEmlPath('/home/me/mail/note.eml'), '/home/me/mail/note.eml')
  // A Windows path starts with a drive letter, which looks like a URL scheme.
  assert.equal(toEmlPath('C:\\Users\\me\\Mail\\note.eml'), 'C:\\Users\\me\\Mail\\note.eml')
  assert.equal(toEmlPath('D:/mail/note.MSG'), 'D:/mail/note.MSG')
})

test('toEmlPath ignores anything that is not a message file', () => {
  assert.equal(toEmlPath('mailto:someone@example.com'), null)
  assert.equal(toEmlPath('guvercin://attach-file?path=%2Ftmp%2Fx'), null)
  assert.equal(toEmlPath('https://example.com/note.eml'), null)
  assert.equal(toEmlPath('/home/me/report.pdf'), null)
  assert.equal(toEmlPath('   '), null)
  assert.equal(toEmlPath(null), null)
})
