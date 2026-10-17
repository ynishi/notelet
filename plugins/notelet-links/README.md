# notelet-links

A Claude Code plugin that shows links to the notelet notes a session wrote.

## What it does

A `tool.call` hook records each successful call of these tools on a notelet MCP server (`mcp__notelet__*` or `mcp__notelet-local__*`):

- `note_put`, `note_append`, `note_delete`
- `note_get` with `to_file` (a plain `note_get` is a read and is not recorded)

Path and version come from the tool's JSON result. Newest first, one entry per path, at most 30. Error and non-JSON results are ignored.

- A band above the prompt shows the last three as `path@vN`, linked to `<ui_base_url>/ui/note/<path>`. A deleted note is shown as `(deleted vN)` without a link.
- `/notes` lists all of them with their URLs and toggles the band between the last three and all.

## Install

```
/plugin marketplace add ynishi/notelet
/plugin install notelet-links@notelet
```

## Setting

Set `ui_base_url` through `/config` (plugin settings): where the notelet browser view is served, without `/ui`, for example `http://127.0.0.1:8080`. On the desktop app a link is drawn only for an `https:` or `http://localhost` address; any other address is shown as plain text. Empty (the default) shows paths without links. No URL is written into the plugin.

## Privacy

Nothing leaves the client. The plugin only reads tool results the session already has; it makes no network request and changes nothing on the server.

## License

MIT OR Apache-2.0, at your option. See `LICENSE-MIT` and `LICENSE-APACHE`.
