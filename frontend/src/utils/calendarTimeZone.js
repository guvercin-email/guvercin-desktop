const HOUR = 3_600_000
const formatters = new Map()

function formatter(timeZone) {
    if (!formatters.has(timeZone)) {
        formatters.set(timeZone, new Intl.DateTimeFormat('en-CA', {
            timeZone, year: 'numeric', month: '2-digit', day: '2-digit',
            hour: '2-digit', minute: '2-digit', hourCycle: 'h23',
        }))
    }
    return formatters.get(timeZone)
}

export function supportedTimeZone(timeZone) {
    try {
        formatter(timeZone)
        return true
    } catch {
        return false
    }
}

export function wallTimeAt(instant, timeZone) {
    const parts = Object.fromEntries(formatter(timeZone).formatToParts(instant).map(({ type, value }) => [type, value]))
    return `${parts.year}-${parts.month}-${parts.day}T${parts.hour}:${parts.minute}`
}

// During overlaps use the first occurrence. In a gap use the offset before
// the transition, which advances the display by the length of the gap.
export function instantForWallTime(value, timeZone) {
    if (!/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(value) || !supportedTimeZone(timeZone)) return null
    const wall = Date.parse(`${value}:00Z`)
    if (!Number.isFinite(wall)) return null
    const offsets = new Set([-36, 0, 36].map((hours) => {
        const sample = wall + hours * HOUR
        return Date.parse(`${wallTimeAt(sample, timeZone)}:00Z`) - sample
    }))
    const candidates = [...offsets].map((offset) => wall - offset).sort((a, b) => a - b)
    return candidates.find((instant) => wallTimeAt(instant, timeZone) === value) ?? candidates.at(-1)
}

export function projectCalendarInstance(instance, displayZone) {
    const { card } = instance
    if (card.allDay || !card.timeZone) return instance
    const start = instantForWallTime(card.start, card.timeZone)
    const end = instantForWallTime(card.end, card.endTimeZone || card.timeZone)
    if (start === null || end === null) return instance
    const realEnd = end > start ? end : start + Math.max(instance.endMs - instance.startMs, 60_000)
    const localStart = wallTimeAt(start, displayZone)
    const localEnd = wallTimeAt(realEnd, displayZone)
    return {
        ...instance, originalCard: card, realStart: start, realEnd,
        card: { ...card, start: localStart, end: localEnd },
        startMs: Date.parse(`${localStart}:00Z`),
        endMs: Math.max(Date.parse(`${localEnd}:00Z`), Date.parse(`${localStart}:00Z`) + 60_000),
    }
}

export function timeZoneObservances(timeZone, startYear) {
    if (timeZone === 'UTC' || !supportedTimeZone(timeZone) || startYear < 1900 || startYear > 2100) return []
    const minute = 60_000
    const offsetAt = (instant) => (Date.parse(`${wallTimeAt(instant, timeZone)}:00Z`) - instant) / minute
    const start = Date.UTC(startYear - 5, 0, 1)
    const end = Date.UTC(startYear + 16, 0, 1)
    let previous = offsetAt(start)
    const observances = [{ onset: wallTimeAt(start, timeZone), from: previous, to: previous }]
    for (let sample = start + 168 * HOUR; sample <= end; sample += 168 * HOUR) {
        const next = offsetAt(sample)
        if (next === previous) continue
        let low = sample - 168 * HOUR
        let high = sample
        while (high - low > minute) {
            const middle = Math.floor((low + high) / (2 * minute)) * minute
            if (offsetAt(middle) === previous) low = middle
            else high = middle
        }
        const onset = new Date(high + previous * minute).toISOString().slice(0, 16)
        observances.push({ onset, from: previous, to: next })
        previous = next
    }
    return observances
}
