import { expect, test } from 'claude-code/testing'
import type { RenderSurface } from 'claude-code'

import { remember, touched, uiUrl } from './register'

const ok = (path: string, version: number) => JSON.stringify({ path, version, size: 1 })

test('put, append and delete on either server are recorded', () => {
  expect(touched('mcp__notelet__note_put', {}, ok('a/b.md', 3))).toEqual({
    path: 'a/b.md',
    version: 3,
    op: 'put',
  })
  expect(touched('mcp__notelet-local__note_append', {}, ok('j.md', 9))?.op).toBe('append')
  expect(touched('mcp__notelet-local__note_delete', {}, ok('x.md', 2))?.op).toBe('delete')
})

test('a plain get is not recorded, a get to_file is', () => {
  expect(touched('mcp__notelet__note_get', {}, ok('a.md', 1))).toBe(null)
  expect(touched('mcp__notelet-local__note_get', { to_file: 'a.md' }, ok('a.md', 1))?.op).toBe('get')
})

test('other tools, non-JSON and error-shaped answers are ignored', () => {
  expect(touched('mcp__other__note_put', {}, ok('a.md', 1))).toBe(null)
  expect(touched('mcp__notelet__note_list', {}, ok('a.md', 1))).toBe(null)
  expect(touched('mcp__notelet__note_put', {}, 'not json')).toBe(null)
  expect(touched('mcp__notelet__note_put', {}, JSON.stringify({ kind: 'Conflict' }))).toBe(null)
  expect(touched('mcp__notelet__note_put', {}, undefined)).toBe(null)
})

test('the /ui URL needs a base and drops trailing slashes', () => {
  expect(uiUrl('', 'a/b.md')).toBe(null)
  expect(uiUrl('http://127.0.0.1:8080/', 'a/b.md')).toBe('http://127.0.0.1:8080/ui/note/a/b.md')
})

test('newest first, one entry per path', () => {
  let list = remember([], { path: 'a.md', version: 1, op: 'put' })
  list = remember(list, { path: 'b.md', version: 1, op: 'put' })
  list = remember(list, { path: 'a.md', version: 2, op: 'append' })
  expect(list.map(n => `${n.path}@${n.version}`)).toEqual(['a.md@2', 'b.md@1'])
})

const BASE = 'http://127.0.0.1:8080'
const answer = (text: string, isError?: boolean) => ({
  ref: { kind: 'text' },
  result: text,
  text,
  ...(isError ? { isError: true } : {}),
})
const notes = ($: any) => $.command.run({ command: 'notes', args: '' })

test('tool.call: an errored answer is not recorded, a successful one is', { options: { ui_base_url: BASE } }, async ($, on) => {
  let isError = true
  on('tool.call', async () => answer(ok('a/b.md', 4), isError) as never)
  await $.tool.call({ tool: 'mcp__notelet__note_put', path: 'a/b.md' } as never)
  expect((await notes($)).text).toContain('No notelet note written')
  isError = false
  await $.tool.call({ tool: 'mcp__notelet-local__note_put', path: 'a/b.md' } as never)
  const out = (await notes($)).text as string
  expect(out).toContain('a/b.md@v4')
  expect(out).toContain(`${BASE}/ui/note/a/b.md`)
})

test('/notes on an empty list does not toggle the band', { options: { ui_base_url: BASE } }, async ($, on) => {
  on('tool.call', async () => answer(ok('a.md', 1)) as never)
  await notes($)
  await $.tool.call({ tool: 'mcp__notelet__note_put', path: 'a.md' } as never)
  expect((await notes($)).text).toContain('band: all')
})

for (const surface of ['terminal', 'desktop'] as const satisfies readonly RenderSurface[]) {
  // the desktop surface draws only https: or http://localhost links
  const base = surface === 'desktop' ? 'http://localhost:8080' : BASE
  test(`AbovePrompt on ${surface}: nothing, then a link`, { options: { ui_base_url: base } }, async ($, on) => {
    on('tool.call', async () => answer(ok('a/b.md', 2)) as never)
    // the engine's own drawing, beneath the plugin: what next(e) reaches
    on('ui.render', async ($$, e) => {
      const { Text } = $$.ui.resolve(e)
      return Text({ children: 'engine' }) as never
    })
    const target = {
      plugin: 'notelet-links',
      surface,
      component: 'AbovePrompt',
      props: { hasSurvey: false },
    } as never
    const empty = await ($.ui as any).mount(target)
    expect(await empty.find({ type: 'Link' })).toBe(undefined)
    expect(await empty.find({ text: 'engine' })).toBeDefined()
    await $.tool.call({ tool: 'mcp__notelet__note_put', path: 'a/b.md' } as never)
    const shown = await ($.ui as any).mount(target)
    const link = await shown.find({ type: 'Link' })
    expect(link?.props.href).toBe(`${base}/ui/note/a/b.md`)
  })
}
