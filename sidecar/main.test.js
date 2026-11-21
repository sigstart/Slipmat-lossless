// SPDX-FileCopyrightText: 2026 Miguel Rincon
// SPDX-License-Identifier: GPL-3.0-or-later

const assert = require('node:assert/strict')
const { EventEmitter } = require('node:events')
const fs = require('node:fs')
const path = require('node:path')
const { PassThrough } = require('node:stream')
const test = require('node:test')
const vm = require('node:vm')

async function boot(env = {}) {
  let widevineStarts = 0
  const blockers = new Set()
  let nextId = 0
  const ipcMain = new EventEmitter()
  const stdin = new PassThrough()
  const rendererSends = []
  let stdout = ''
  let window
  class BrowserWindow extends EventEmitter {
    constructor() {
      super()
      window = this
      this.webContents = Object.assign(new EventEmitter(), {
        setWindowOpenHandler() {},
        setAudioMuted(muted) { this.muted = muted },
        session: { setPermissionRequestHandler() {} },
        send(channel, ...args) { rendererSends.push([channel, ...structuredClone(args)]) },
      })
    }
    async loadURL() {}
    hide() {}
  }
  const app = Object.assign(new EventEmitter(), {
    setName() {},
    setDesktopName() {},
    commandLine: { appendSwitch() {}, getSwitchValue() { return '' } },
    whenReady: () => Promise.resolve(),
  })
  const electron = {
    app, BrowserWindow, ipcMain,
    components: { whenReady: async () => { widevineStarts++ }, status: () => ({}) },
    powerSaveBlocker: {
      start(type) {
        assert.equal(type, 'prevent-app-suspension')
        blockers.add(++nextId)
        return nextId
      },
      stop(id) { assert.equal(blockers.delete(id), true) },
    },
  }
  vm.runInNewContext(fs.readFileSync(path.join(__dirname, 'main.js'), 'utf8'), {
    require: (name) => name === 'electron' ? electron : require(name),
    __dirname,
    process: {
      argv: [], env, platform: 'linux', stdin,
      stdout: { write(chunk) { stdout += chunk } },
      stderr: { write() {} }, on() {},
    },
    setInterval() {}, clearInterval() {},
  })
  await new Promise(setImmediate)
  return {
    blockers, window, rendererSends, widevineStarts,
    command: (msg) => stdin.write(JSON.stringify(msg) + '\n'),
    event: (ev) => ipcMain.emit('slipmat:event', {}, structuredClone(ev)),
    stdoutFrames() {
      const lines = stdout.split('\n')
      assert.equal(lines.pop(), '', 'stdout must end at an NDJSON frame boundary')
      return lines.map((line) => JSON.parse(line))
    },
  }
}

test('sleep is inhibited only while playback needs it', async () => {
  const { blockers, window, event } = await boot()
  assert.equal(blockers.size, 0, 'hidden startup must allow sleep')
  for (const state of ['loading', 'playing', 'playing', 'seeking', 'waiting', 'stalled']) {
    event({ event: 'playbackState', state })
    assert.equal(blockers.size, 1, state)
  }
  for (const state of ['paused', 'stopped', 'ended', 'completed', 'none', 'unknown']) {
    event({ event: 'playbackState', state: 'playing' })
    event({ event: 'playbackState', state })
    assert.equal(blockers.size, 0, state)
  }
  event({ event: 'playbackState', state: 'playing' })
  window.webContents.emit('did-start-navigation', { isMainFrame: true, isSameDocument: true })
  assert.equal(blockers.size, 1, 'SPA navigation must preserve playback')
  window.webContents.emit('did-start-navigation', { isMainFrame: true, isSameDocument: false })
  assert.equal(blockers.size, 0, 'document replacement releases the inhibitor')
  event({ event: 'playbackState', state: 'playing' })
  event({ event: 'hook-failed' })
  assert.equal(blockers.size, 0, 'failed hook must allow sleep')
  event({ event: 'playbackState', state: 'playing' })
  window.webContents.emit('render-process-gone')
  assert.equal(blockers.size, 0, 'dead renderer must allow sleep')
})

test('playback protocol restores an idle queue and relays an independent duplicate-track advance', async () => {
  const { command, event, rendererSends, stdoutFrames } = await boot()
  const start = stdoutFrames().length
  const restore = {
    cmd: 'setQueue', songs: ['A', 'B', 'B'], startPosition: 0,
    startPlaying: false, startTimeMs: 24_000,
  }
  const queued = { event: 'cmd-queued', cmd: 'setQueue', depth: 1 }
  command(restore)
  assert.deepEqual(rendererSends, [], 'restore must wait for the hook')
  assert.deepEqual(stdoutFrames().slice(start), [queued])

  const ready = { event: 'hook-ready', authorized: true, version: 'synthetic' }
  event(ready)
  assert.deepEqual(rendererSends, [['slipmat:command', restore]])

  const items = [
    { id: 'A', catalogId: 'A', occurrenceId: 'a-1' },
    { id: 'B', catalogId: 'B', occurrenceId: 'b-1' },
    { id: 'B', catalogId: 'B', occurrenceId: 'b-2' },
  ]
  const restoredQueue = { event: 'queue', reason: 'items', position: -1, items }
  event(restoredQueue)

  const firstTrack = {
    event: 'nowPlaying', item: items[0], queue: { reason: 'position', position: 0, items },
  }
  const firstBQueue = { reason: 'position', position: 1, items }
  const firstB = { event: 'nowPlaying', item: items[1], queue: firstBQueue }
  const playing = { event: 'playbackState', state: 'playing' }
  const paused = { event: 'playbackState', state: 'paused' }
  const seeked = { event: 'position', positionMs: 42_000, durationMs: 120_000 }
  const transport = [
    [{ cmd: 'changeToIndex', index: restore.startPosition }],
    [{ cmd: 'play' }, firstTrack, playing],
    [{ cmd: 'pause' }, paused],
    [{ cmd: 'seek', positionMs: 42_000 }, seeked],
    [{ cmd: 'play' }, playing],
    [{ cmd: 'next' }, { event: 'queue', ...firstBQueue }, firstB],
  ]
  for (const [msg, ...events] of transport) {
    command(msg)
    assert.deepEqual(rendererSends.at(-1), ['slipmat:command', msg])
    for (const frame of events) event(frame)
  }
  const expectedCommands = [restore, ...transport.map(([msg]) => msg)]
    .map((msg) => ['slipmat:command', msg])
  assert.deepEqual(rendererSends, expectedCommands)

  const secondBQueue = { reason: 'position', position: 2, items }
  const advance = { event: 'queue', ...secondBQueue }
  const secondB = { event: 'nowPlaying', item: items[2], queue: secondBQueue }
  event(advance)
  event(secondB)
  await new Promise(setImmediate)

  assert.deepEqual(stdoutFrames().slice(start), [
    queued, ready, restoredQueue, ...transport.flatMap(([, ...events]) => events), advance, secondB,
  ])
  assert.deepEqual(rendererSends, expectedCommands, 'a natural advance must send no command')
})

test('metadata mode mutes the window and forwards library commands without Widevine', async () => {
  const { blockers, window, rendererSends, widevineStarts, command, event } =
    await boot({ SLIPMAT_METADATA_ONLY: '1' })
  assert.equal(widevineStarts, 0)
  assert.equal(window.webContents.muted, true)
  command({ cmd: 'setQueue', songs: ['123'] })
  command({ cmd: 'refreshTokens' })
  event({ event: 'hook-ready', authorized: true, version: 'synthetic' })
  command({ cmd: 'play' })
  event({ event: 'playbackState', state: 'playing' })
  assert.deepEqual(rendererSends, [['slipmat:command', { cmd: 'refreshTokens' }]])
  assert.equal(blockers.size, 0)
})
