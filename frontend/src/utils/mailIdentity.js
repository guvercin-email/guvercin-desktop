export function mailboxIdentity(mailbox) {
  return /^inbox$/i.test(mailbox) ? 'INBOX' : mailbox
}

export function dedupeMailboxes(mailboxes) {
  const unique = new Map()
  for (const mailbox of mailboxes || []) {
    if (typeof mailbox !== 'string' || !mailbox) continue
    const key = mailboxIdentity(mailbox)
    if (!unique.has(key)) unique.set(key, mailbox)
  }
  return [...unique.values()]
}

export function mailKey(mail, fallbackMailbox = 'INBOX') {
  if (mail?.id == null || mail.id === '') return ''
  return JSON.stringify([mailboxIdentity(mail.mailbox || fallbackMailbox || 'INBOX'), String(mail.id), mail.uid_validity ?? 0])
}

export function sameMail(first, second, fallbackMailbox) {
  return Boolean(mailKey(first, fallbackMailbox)) && mailKey(first, fallbackMailbox) === mailKey(second, fallbackMailbox)
}

export async function queueMailActions(keys, queueAction, type, payload = {}) {
  const uniqueKeys = [...new Set(keys.filter(Boolean))]
  const results = await Promise.allSettled(uniqueKeys.map(async (key) => {
    const [mailbox, uid, uidValidity = 0] = JSON.parse(key)
    await queueAction(type, uid, payload, mailbox, uidValidity)
  }))
  return uniqueKeys.filter((_, index) => results[index].status === 'rejected')
}

export function mailQuery(mailbox, uidValidity) {
  return new URLSearchParams({ mailbox: mailbox || 'INBOX', uid_validity: String(uidValidity ?? 0) }).toString()
}
