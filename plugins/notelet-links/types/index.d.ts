export type Touched = {
  /** Note path, e.g. `projects/x/drafts/y.md`. */
  path: string
  /** Version the write answered (for a delete: the version removed). */
  version: number
  /** The tool that touched it. */
  op: 'put' | 'append' | 'get' | 'delete'
}

declare module 'claude-code' {
  interface PluginState {
    'notelet-links': { notes: readonly Touched[]; isExpanded: boolean }
  }
}
