import { atom, read, update } from 'claude-code'
import type { Register } from 'claude-code'

import type { Touched } from '../types'

const notes = atom({ plugin: 'notelet-links', key: 'notes' } as const, [] as readonly Touched[])
const isExpanded = atom({ plugin: 'notelet-links', key: 'isExpanded' } as const, false)

/** notelet MCP tools on either server, by the op they record. */
const TOOL = /^mcp__notelet(?:-local)?__note_(put|append|get|delete)$/

/** How many notes the band shows folded, and the most kept at all. */
const FOLDED = 3
const KEPT = 30

/**
 * What a successful notelet call touched, or null. `note_get` counts only
 * with `to_file` (a read into a file to edit); a plain read is not a write.
 */
export function touched(
  tool: string,
  args: Record<string, unknown>,
  text: string | undefined,
): Touched | null {
  const m = TOOL.exec(tool)
  if (!m || text === undefined) return null
  const op = m[1] as Touched['op']
  if (op === 'get' && typeof args.to_file !== 'string') return null
  let out: unknown
  try {
    out = JSON.parse(text)
  } catch {
    return null
  }
  if (typeof out !== 'object' || out === null) return null
  const { path, version } = out as { path?: unknown; version?: unknown }
  if (typeof path !== 'string' || typeof version !== 'number') return null
  return { path, version, op }
}

/** The /ui page of a note, or null without a base URL. */
export function uiUrl(base: string, path: string): string | null {
  const b = base.trim().replace(/\/+$/, '')
  return b === '' ? null : `${b}/ui/note/${path}`
}

/** Newest first, one entry per path, at most KEPT. */
export function remember(list: readonly Touched[], t: Touched): readonly Touched[] {
  return [t, ...list.filter(n => n.path !== t.path)].slice(0, KEPT)
}

function label(n: Touched): string {
  return n.op === 'delete' ? `${n.path} (deleted v${n.version})` : `${n.path}@v${n.version}`
}

export const register: Register = (on, options) => {
  const base = typeof options.ui_base_url === 'string' ? options.ui_base_url : ''

  on('session.start', async ($, e, next) => {
    await $.command.register({
      name: 'notes',
      description: 'List the notelet notes this session wrote, with /ui links',
    })
    return next(e)
  })

  on('command.run', { command: 'notes' }, async $ => {
    const list = await read($, notes)
    if (list.length === 0) return { text: 'No notelet note written in this session yet.' }
    const expanded = await update($, isExpanded, v => !v)
    const lines = list.map(n => {
      const url = n.op === 'delete' ? null : uiUrl(base, n.path)
      return url ? `- ${label(n)}  ${url}` : `- ${label(n)}`
    })
    const hint = base.trim() === '' ? '\n(set ui_base_url in /config for links)' : ''
    const mode = expanded ? 'band: all' : `band: last ${FOLDED}`
    return { text: `notelet notes in this session (${mode}):\n${lines.join('\n')}${hint}` }
  })

  on('tool.call', async ($, e, next) => {
    const ran = await next(e)
    if (ran.deny === undefined && ran.isError !== true) {
      const t = touched(e.tool, e as Record<string, unknown>, ran.text)
      if (t) await update($, notes, list => remember(list, t))
    }
    return ran
  }).catch(($, e, next) => next(e)) // a failed record must not change the call's result

  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    const list = await read($, notes)
    if (e.props.hasSurvey || list.length === 0) return next(e)
    const expanded = await read($, isExpanded)
    const shown = expanded ? list : list.slice(0, FOLDED)
    const { Box, Text, Link } = $.ui.resolve(e)

    return (
      <Box flexDirection="column">
        {shown.map(n => {
          const url = n.op === 'delete' ? null : uiUrl(base, n.path)
          return (
            <Box key={n.path}>
              <Text dimColor>notelet </Text>
              {url ? <Link href={url} label={label(n)} /> : <Text>{label(n)}</Text>}
            </Box>
          )
        })}
        {!expanded && list.length > FOLDED ? (
          <Text dimColor>+{list.length - FOLDED} more: /notes</Text>
        ) : null}
      </Box>
    )
  })
}
