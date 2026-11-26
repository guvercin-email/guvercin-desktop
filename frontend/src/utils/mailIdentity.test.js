import assert from 'node:assert/strict'
import test from 'node:test'
import { mailKey, queueMailActions, sameMail } from './mailIdentity.js'

test('mail identity distinguishes equal UIDs in different mailboxes', () => {
  assert.equal(sameMail({ id: 42 }, { id: '42', mailbox: 'INBOX' }), true)
  assert.equal(sameMail({ id: '42', mailbox: 'Archive' }, { id: '42', mailbox: 'INBOX' }), false)
  assert.equal(sameMail(null, {}), false)
})

test('batch actions preserve mailbox names and report only failed targets', async () => {
  const inbox = mailKey({ id: '42', mailbox: 'INBOX' })
  const archive = mailKey({ id: '42', mailbox: 'Projects, 2026' })
  const calls = []
  const payload = { destination: 'Trash' }
  const failed = await queueMailActions([inbox, archive, inbox], async (...args) => {
    calls.push(args)
    if (args[3] === 'Projects, 2026') throw new Error('Queue unavailable')
  }, 'move', payload)

  assert.deepEqual(calls, [
    ['move', '42', payload, 'INBOX'],
    ['move', '42', payload, 'Projects, 2026'],
  ])
  assert.deepEqual(failed, [archive])
})
