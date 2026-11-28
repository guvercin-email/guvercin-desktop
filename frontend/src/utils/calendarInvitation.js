const EMAIL = /^[^\s<>,;:"()@]+@[^\s<>,;:"()@]+$/u
const DATE_VALUE = /^\d{8}(?:T\d{6}Z?)?$/
const calendarAddress = (value) => {
    if (!/^mailto:/i.test(value)) return ''
    try {
        const address = decodeURIComponent(value.slice(7))
        return EMAIL.test(address) ? address : ''
    } catch {
        return ''
    }
}

function property(line) {
    const parts = line.match(/^([\w-]+)((?:;(?:[^";:]|"[^"]*")+)*):([^\r\n]*)$/)
    return parts && { name: parts[1].toUpperCase(), params: parts[2], value: parts[3] }
}

export function parseCalendarInvitations(source, email) {
    if (!EMAIL.test(email) || source.length > 262144) return []
    const lines = source.replace(/\r?\n[ \t]/g, '').split(/\r?\n/)
    const invitations = []
    const timezones = []
    const stack = []
    let event
    let timezone
    let isRequest = false
    for (const line of lines) {
        const node = property(line)
        if (!node) continue
        const value = node.value.toUpperCase()
        if (node.name === 'BEGIN') {
            if (stack.length === 1 && stack[0] === 'VCALENDAR') {
                if (value === 'VEVENT') event = []
                if (value === 'VTIMEZONE') timezone = []
            }
            stack.push(value)
        }
        if (timezone) timezone.push(line)
        if (node.name === 'END') {
            if (stack.pop() !== value) return []
            if (stack.length === 1 && value === 'VTIMEZONE' && timezone) {
                timezones.push(timezone.join('\r\n'))
                timezone = null
            }
            if (stack.length === 1 && value === 'VEVENT' && event) {
                const attendee = event.filter(({ name }) => name === 'ATTENDEE')
                    .map(({ value }) => calendarAddress(value)).find((address) => address.toLowerCase() === email.toLowerCase())
                const get = (name) => event.find((field) => field.name === name)
                const organizer = calendarAddress(get('ORGANIZER')?.value || '')
                const uid = get('UID')?.value
                const sequence = get('SEQUENCE')?.value || '0'
                const recurrence = get('RECURRENCE-ID')
                if (organizer && attendee && uid && uid.length <= 512 && /^\d{1,10}$/.test(sequence)
                    && (!recurrence || DATE_VALUE.test(recurrence.value))) {
                    invitations.push({ organizer, attendee, uid, sequence, recurrence, title: get('SUMMARY')?.value || '' })
                }
                event = null
            }
        } else if (stack.length === 1 && stack[0] === 'VCALENDAR' && node.name === 'METHOD') {
            isRequest = value === 'REQUEST'
        } else if (stack.length === 2 && stack[1] === 'VEVENT' && event && node.name !== 'BEGIN') {
            event.push(node)
        }
    }
    return isRequest && !stack.length ? invitations.map((invitation) => ({ ...invitation, timezones })) : []
}

function foldLine(line) {
    const encoder = new TextEncoder()
    const chunks = []
    let current = ''
    let size = 0
    for (const character of line) {
        const bytes = encoder.encode(character).length
        if (size + bytes > 75) {
            chunks.push(current)
            current = ' '
            size = 1
        }
        current += character
        size += bytes
    }
    return [...chunks, current].join('\r\n')
}

export function calendarInvitationReply(invitation, status, now = new Date()) {
    if (!['ACCEPTED', 'TENTATIVE', 'DECLINED'].includes(status)) throw new Error('Invalid invitation response')
    const { organizer, attendee, uid, sequence, recurrence, timezones } = invitation
    const zone = recurrence?.params.match(/;TZID=(?:"([^"]+)"|([^;]+))/i)
    const tzid = zone?.[1] || zone?.[2]
    const timezone = tzid && timezones.find((definition) => definition.split('\r\n').some((line) =>
        property(line)?.name === 'TZID' && property(line)?.value === tzid))
    const lines = [
        'BEGIN:VCALENDAR', 'PRODID:-//Guvercin//Calendar//EN', 'VERSION:2.0', 'METHOD:REPLY',
        ...(timezone ? timezone.split('\r\n') : []),
        'BEGIN:VEVENT', `UID:${uid}`, `SEQUENCE:${sequence}`,
        `DTSTAMP:${now.toISOString().replace(/[-:]/g, '').replace(/\.\d+Z$/, 'Z')}`,
        `ORGANIZER:mailto:${organizer}`, `ATTENDEE;PARTSTAT=${status}:mailto:${attendee}`,
        ...(recurrence ? [`RECURRENCE-ID${recurrence.params}:${recurrence.value}`] : []),
        'END:VEVENT', 'END:VCALENDAR',
    ]
    return lines.map(foldLine).join('\r\n') + '\r\n'
}
