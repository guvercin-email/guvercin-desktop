import test from 'node:test'
import assert from 'node:assert/strict'
import { instantForWallTime, projectCalendarInstance, timeZoneObservances } from './calendarTimeZone.js'

test('weekly wall time follows DST in the event zone', () => {
    const timeZone = 'America/New_York'
    const instance = (start, end) => ({
        card: { start, end, timeZone }, startMs: 0, endMs: 3_600_000, instanceKey: '1@123',
    })
    const before = projectCalendarInstance(instance('2026-03-02T09:00', '2026-03-02T10:00'), 'Europe/Istanbul')
    const after = projectCalendarInstance(instance('2026-03-09T09:00', '2026-03-09T10:00'), 'Europe/Istanbul')
    assert.equal(before.card.start, '2026-03-02T17:00')
    assert.equal(after.card.start, '2026-03-09T16:00')
    assert.equal(after.instanceKey, '1@123')
    assert.equal(after.originalCard.start, '2026-03-09T09:00')
})

test('wall-time conversion resolves the spring gap and fall overlap', () => {
    assert.equal(instantForWallTime('2026-03-08T02:30', 'America/New_York'), Date.parse('2026-03-08T07:30:00Z'))
    assert.equal(instantForWallTime('2026-11-01T01:30', 'America/New_York'), Date.parse('2026-11-01T05:30:00Z'))
    const zones = timeZoneObservances('America/New_York', 2026)
    assert.ok(zones.some(({ onset, from, to }) => onset === '2026-03-08T02:00' && from === -300 && to === -240))
})
