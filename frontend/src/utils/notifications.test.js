import test from 'node:test'
import assert from 'node:assert/strict'

const { currentPlatform, notificationSoundName } = await import('./notifications.js')

test('currentPlatform reads the three desktop user agents', () => {
  assert.equal(currentPlatform('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)'), 'macos')
  assert.equal(currentPlatform('Mozilla/5.0 (Windows NT 10.0; Win64; x64)'), 'windows')
  assert.equal(currentPlatform('Mozilla/5.0 (X11; Linux x86_64)'), 'linux')
  assert.equal(currentPlatform(''), 'linux')
})

test('notificationSoundName speaks each platform vocabulary', () => {
  assert.equal(
    notificationSoundName('mail', 'macos'),
    'NSUserNotificationDefaultSoundName',
  )
  // Windows only accepts its fixed toast sound names; anything else is dropped.
  assert.equal(notificationSoundName('mail', 'windows'), 'Mail')
  assert.equal(notificationSoundName('reminder', 'windows'), 'Reminder')
  // Linux takes XDG sound-naming-spec names from the user's sound theme.
  assert.equal(notificationSoundName('mail', 'linux'), 'message-new-instant')
  assert.equal(notificationSoundName('reminder', 'linux'), 'alarm-clock-elapsed')
})
