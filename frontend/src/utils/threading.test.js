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

test('parent-only headers join a chain even when the first message is missing', () => {
  const mails = [
    { id: '3', message_id: '<third@test>', in_reply_to: '<second@test>' },
    { id: '1', message_id: '<first@test>', in_reply_to: '<missing@test>' },
    { id: '2', message_id: '<second@test>', in_reply_to: '<first@test>' },
  ]
  const [thread, ...others] = buildThreads(mails)
  assert.equal(thread.id, '<missing@test>')
  assert.deepEqual(thread.mail_ids, ['1', '2', '3'])
  assert.equal(others.length, 0)
})

test('cyclic reply headers produce a single stable group', () => {
  const mails = [
    { id: '1', message_id: '<a@test>', in_reply_to: '<b@test>' },
    { id: '2', message_id: '<b@test>', in_reply_to: '<a@test>' },
  ]
  assert.deepEqual(buildThreads(mails).map((thread) => thread.id), ['<a@test>'])
  assert.deepEqual(buildThreads(mails.toReversed()).map((thread) => thread.id), ['<a@test>'])
})

test('subject fallback keeps case-distinct folders separate', () => {
  const mail = { id: '42', address: 'sender@example.test', subject: 'Same subject' }
  assert.equal(buildThreads([{ ...mail, mailbox: 'Archive' }, { ...mail, mailbox: 'archive' }]).length, 2)
})
