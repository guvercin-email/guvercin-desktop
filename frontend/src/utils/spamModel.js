const MIN_EXAMPLES = 3
const MAX_FEATURES = 48
export const MAX_SPAM_EXAMPLES = 600

export function spamFeatures(mail, body = '') {
    const sender = (mail.address || '').trim().toLowerCase()
    const domain = sender.split('@').at(-1)
    const subject = (mail.subject || '').toLowerCase()
    const terms = subject.match(/[\p{L}\p{N}]{3,}/gu) || []
    const bodyTerms = body.slice(0, 32768).toLowerCase().match(/[\p{L}\p{N}]{3,}/gu) || []
    return [...new Set([
        ...(sender ? [`sender:${sender}`, `domain:${domain}`] : []),
        ...terms.slice(0, MAX_FEATURES - 2).map((term) => `subject:${[...term].slice(0, 48).join('')}`),
        ...bodyTerms.slice(0, 64).map((term) => `body:${[...term].slice(0, 48).join('')}`),
    ])]
}

export function buildSpamModel(examples) {
    const counts = new Map()
    let spam = 0
    let clean = 0
    for (const example of examples) {
        if (!Array.isArray(example.features) || !['spam', 'clean'].includes(example.label)) continue
        const index = example.label === 'spam' ? 0 : 1
        if (index === 0) spam++
        else clean++
        for (const feature of new Set(example.features)) {
            if (!counts.has(feature)) counts.set(feature, [0, 0])
            counts.get(feature)[index]++
        }
    }
    return { counts, spam, clean }
}

export function isLikelySpam(mail, key, examplesByKey, model) {
    const feedback = examplesByKey.get(key)
    if (feedback) return feedback.label === 'spam'
    if (model.spam < MIN_EXAMPLES || model.clean < MIN_EXAMPLES) return false
    let score = 0
    let evidence = 0
    for (const feature of spamFeatures(mail)) {
        const counts = model.counts.get(feature)
        if (!counts || counts[0] + counts[1] < 2) continue
        const [spam, clean] = counts
        score += Math.log(((spam + 1) / (model.spam + 2)) / ((clean + 1) / (model.clean + 2)))
        evidence++
    }
    return evidence >= 2 && score >= Math.log(5)
}
