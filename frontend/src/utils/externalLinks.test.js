import test from 'node:test'
import assert from 'node:assert/strict'
import { JSDOM } from 'jsdom'
import { sanitizeMailHtml } from './externalLinks.js'

function withDom(cb) {
  const win = new JSDOM('<!doctype html><html><body></body></html>').window
  const oldDOMParser = global.DOMParser
  try {
    global.DOMParser = win.DOMParser
    cb()
  } finally {
    if (oldDOMParser === undefined) delete global.DOMParser
    else global.DOMParser = oldDOMParser
  }
}

const ALLOW = { remoteImages: 'auto' }

test('sanitizeMailHtml moves first srcset candidate to src when src is missing', () => {
  withDom(() => {
    const html = '<img srcset="https://example.com/a.jpg 1x, https://example.com/b.jpg 2x" loading="lazy">'
    const out = sanitizeMailHtml(html, ALLOW)
    assert.match(out, /src="https:\/\/example\.com\/a\.jpg"/)
    assert.doesNotMatch(out, /srcset=/)
    assert.doesNotMatch(out, /loading=/)
  })
})

test('sanitizeMailHtml ignores javascript: srcset candidates and removes srcset', () => {
  withDom(() => {
    const html = '<img srcset="javascript:alert(1), https://example.com/c.jpg 2x">'
    const out = sanitizeMailHtml(html, ALLOW)
    assert.doesNotMatch(out, /src="javascript:/)
    // srcset should be removed even if no src was produced
    assert.doesNotMatch(out, /srcset=/)
  })
})

test('remote images are withheld by default', () => {
  withDom(() => {
    const out = sanitizeMailHtml('<img src="https://tracker.example/pixel.gif">')
    // The live src is gone; only the parked copy remains.
    assert.doesNotMatch(out, /(?<!data-blocked-)src="https:\/\/tracker\.example/)
    assert.match(out, /data-blocked-src="https:\/\/tracker\.example\/pixel\.gif"/)
    assert.match(out, /data-blocked-remote="1"/)
  })
})

test('omitting the option withholds rather than loads', () => {
  withDom(() => {
    // A caller that forgets to pass the account preference must not leak.
    const out = sanitizeMailHtml('<img src="//tracker.example/pixel.gif">')
    assert.match(out, /data-blocked-src/)
  })
})

test('inline url() backgrounds and remote stylesheets are withheld too', () => {
  withDom(() => {
    const html = '<link rel="stylesheet" href="https://tracker.example/s.css">'
      + '<div style="background-image:url(https://tracker.example/bg.png);color:red">x</div>'
    const out = sanitizeMailHtml(html, { remoteImages: 'block' })
    assert.doesNotMatch(out, /tracker\.example\/s\.css/)
    // The live style attribute no longer reaches out; unrelated declarations
    // survive, and the original is parked for "load images".
    assert.match(out, /style="background-image:none;\s*color:red"/)
    assert.match(out, /data-blocked-style="background-image:url\(https:\/\/tracker\.example/)
  })
})

test('data: and cid: images are never withheld', () => {
  withDom(() => {
    const html = '<img src="cid:logo@1"><img src="data:image/gif;base64,R0lGOD">'
    const out = sanitizeMailHtml(html)
    assert.match(out, /src="cid:logo@1"/)
    assert.match(out, /src="data:image\/gif/)
    assert.doesNotMatch(out, /data-blocked-remote/)
  })
})

test('prompt mode adds a load-images bar, block mode does not', () => {
  withDom(() => {
    const html = '<img src="https://tracker.example/pixel.gif">'
    const prompt = sanitizeMailHtml(html, {
      remoteImages: 'prompt',
      labels: { blocked: 'Blocked here', load: 'Load them' },
    })
    assert.match(prompt, /__guvercin_remote_bar/)
    assert.match(prompt, /Blocked here/)
    assert.match(prompt, /Load them/)

    const blocked = sanitizeMailHtml(html, { remoteImages: 'block' })
    assert.doesNotMatch(blocked, /__guvercin_remote_bar/)
  })
})

test('allowing remote images leaves the document untouched', () => {
  withDom(() => {
    const out = sanitizeMailHtml('<img src="https://example.com/a.jpg">', ALLOW)
    assert.match(out, /src="https:\/\/example\.com\/a\.jpg"/)
    assert.doesNotMatch(out, /data-blocked/)
    assert.doesNotMatch(out, /__guvercin_remote_bar/)
  })
})

test('scripts the message brings with it are still removed in prompt mode', () => {
  withDom(() => {
    const html = '<script>steal()</script><img src="https://tracker.example/p.gif" onerror="steal()">'
    const out = sanitizeMailHtml(html, { remoteImages: 'prompt' })
    assert.doesNotMatch(out, /steal\(\)/)
  })
})


test('mail documents have a restrictive policy and no active navigation markup', () => {
  withDom(() => {
    const input = '<meta http-equiv="refresh" content="0;url=https://example.test/">'
      + '<base href="https://example.test/"><svg><animate attributeName="href"></animate></svg>'
      + '<a href="data:text/html,content">Unsafe link</a><a href="https://example.test/">Safe link</a>'
    const out = new DOMParser().parseFromString(sanitizeMailHtml(input), 'text/html')
    assert.equal(out.querySelector('base, svg, meta[http-equiv="refresh"]'), null)
    assert.equal(out.querySelector('a').hasAttribute('href'), false)
    assert.equal(out.querySelectorAll('a')[1].dataset.externalHref, 'https://example.test/')
    assert.match(out.querySelector('meta[http-equiv="Content-Security-Policy"]').content, /default-src 'none'/)
  })
})

test('cached inline attachments work with relative and legacy backend URLs while remote images are blocked', () => {
  withDom(() => {
    const path = '/api/offline/1/local-content/42/attachments/0?mailbox=INBOX'
    const html = `<img src="${path}"><img src="http://127.0.0.1:5000${path}">`
    const out = new DOMParser().parseFromString(sanitizeMailHtml(html), 'text/html')
    assert.equal(out.images[0].getAttribute('src'), out.images[1].getAttribute('src'))
    assert.equal(new URL(out.images[0].src).pathname, path.split('?')[0])
    assert.equal(out.querySelector('[data-blocked-src]'), null)
  })
})
