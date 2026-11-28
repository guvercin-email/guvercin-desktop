import test from 'node:test'
import assert from 'node:assert/strict'
import { calendarInvitationReply, parseCalendarInvitations } from './calendarInvitation.js'

const request = [
    'BEGIN:VCALENDAR',
    'VERSION:2.0',
    'METHOD:REQUEST',
    'BEGIN:VEVENT',
    'UID:meeting-17',
    'ORGANIZER:mailto:organizer@example.com',
    'ATTENDEE;CN="Ada: Example";RSVP=TRUE:mailto:ada@example.com',
    'ATTENDEE:mailto:other@example.com',
    'RECURRENCE-ID;VALUE=DATE:20261129',
    'SUMMARY:Sun',
    ' day meeting',
    'SEQUENCE:2',
    'END:VEVENT',
    'END:VCALENDAR',
].join('\r\n')

test('an invited account can reply to an individual occurrence', () => {
    const [invitation] = parseCalendarInvitations(request, 'Ada@example.com')
    assert.equal(invitation.title, 'Sunday meeting')
    const reply = calendarInvitationReply(invitation, 'TENTATIVE', new Date('2026-11-28T12:00:00Z'))
    assert.match(reply, /METHOD:REPLY\r\n/)
    assert.match(reply, /UID:meeting-17\r\nSEQUENCE:2\r\nDTSTAMP:20261128T120000Z\r\n/)
    assert.match(reply, /ATTENDEE;PARTSTAT=TENTATIVE:mailto:ada@example.com\r\n/)
    assert.match(reply, /RECURRENCE-ID;VALUE=DATE:20261129\r\n/)
    assert.doesNotMatch(reply, /other@example.com/)
})

test('uninvited accounts and calendar cancellations cannot be answered', () => {
    assert.deepEqual(parseCalendarInvitations(request, 'someone@example.com'), [])
    assert.deepEqual(parseCalendarInvitations(request.replace('METHOD:REQUEST', 'METHOD:CANCEL'), 'ada@example.com'), [])
})
