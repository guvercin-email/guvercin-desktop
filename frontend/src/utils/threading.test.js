import assert from 'node:assert/strict'
import test from 'node:test'
import { buildThreads } from './threading.js'

test('the original message joins replies from other senders', () => {
  const original = { id: '1', message_id: '<original@example.com>', address: 'a@example.com', subject: 'Planning' }
  const reply = { id: '2', references: original.message_id, address: 'b@example.com', subject: 'Re: Planning' }
  const threads = buildThreads([reply, original], { mailbox: 'INBOX' })
  assert.equal(threads.length, 1)
  assert.deepEqual(threads[0].mails, [original, reply])
})

test('unreferenced messages retain sender-and-subject grouping', () => {
  const first = { id: '1', message_id: '<first@example.com>', address: 'a@example.com', subject: 'Planning' }
  const second = { ...first, id: '2', message_id: '<second@example.com>', subject: 'Re: Planning' }
  assert.equal(buildThreads([first, second], { mailbox: 'INBOX' }).length, 1)
})
