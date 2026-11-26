export function mailKey(mail, fallbackMailbox = 'INBOX') {
  if (mail?.id == null || mail.id === '') return ''
  return JSON.stringify([mail.mailbox || fallbackMailbox || 'INBOX', String(mail.id)])
}

export function sameMail(first, second, fallbackMailbox) {
  return Boolean(mailKey(first, fallbackMailbox)) && mailKey(first, fallbackMailbox) === mailKey(second, fallbackMailbox)
}

export async function queueMailActions(keys, queueAction, type, payload = {}) {
  const uniqueKeys = [...new Set(keys.filter(Boolean))]
  const results = await Promise.allSettled(uniqueKeys.map(async (key) => {
    const [mailbox, uid] = JSON.parse(key)
    await queueAction(type, uid, payload, mailbox)
  }))
  return uniqueKeys.filter((_, index) => results[index].status === 'rejected')
}
