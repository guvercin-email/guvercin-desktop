import assert from 'node:assert/strict'
import test from 'node:test'
import { buildSpamModel, isLikelySpam, spamFeatures } from './spamModel.js'

test('spam model learns repeated signals while giving explicit feedback precedence', () => {
    const spam = { address: 'promo@example.test', subject: 'Exclusive bonus' }
    const clean = { address: 'team@example.test', subject: 'Meeting agenda' }
    const examples = Array.from({ length: 3 }, (_, index) => [
        { key: `spam-${index}`, label: 'spam', features: spamFeatures(spam) },
        { key: `clean-${index}`, label: 'clean', features: spamFeatures(clean) },
    ]).flat()
    const feedback = new Map(examples.map((entry) => [entry.key, entry]))
    const model = buildSpamModel(examples)
    assert.equal(isLikelySpam(spam, 'new-spam', feedback, model), true)
    assert.equal(isLikelySpam(clean, 'new-clean', feedback, model), false)
    assert.equal(isLikelySpam(spam, 'clean-0', feedback, model), false)
    assert.equal(isLikelySpam(spam, 'new-spam', feedback, buildSpamModel(examples.slice(0, 2))), false)
})
