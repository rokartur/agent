# Plugins

A plugin is a directory with a `plugin.json` and the program it names. The agent starts each enabled plugin at startup, and again after it exits when anything changes in `/plugins`. It talks to it in JSON-RPC 2.0 over stdin and stdout, one JSON message per line. When stdin closes, the agent has quit: exit then, because the plugin runs in its own session and gets no SIGHUP. Stderr is not part of the protocol. The agent keeps the last stderr line and shows it in `/plugins` when the plugin exits.

## Install

```sh
agent plugin install usagefleet        # a plugin from this directory, built into the agent
agent plugin install ./my-plugin       # your own plugin directory
agent plugin list
agent plugin remove usagefleet
```

Plugins go to `~/.agents/plugins/<name>/`. In the TUI, `/plugins` installs, removes, enables and disables them. Disabled plugins are kept in `disabled_plugins` in `~/.agents/settings.json`.

At startup, the agent updates each installed plugin from this directory when the agent has a higher `version`, and shows a message. When you change a plugin here, increase its `version`, or users keep the old copy. A local copy with the same or a higher version is not changed.

## plugin.json

```json
{
	"version": "0.1.0",
	"description": "One line for /plugins",
	"author": "octocat",
	"command": ["bun", "index.ts"],
	"commands": [{ "name": "hello", "description": "Shown in the slash menu" }],
	"tools": [{ "name": "lookup", "description": "Shown to the model", "input_schema": { "type": "object", "properties": { "q": { "type": "string" } } } }],
	"settings": [{ "key": "verbose", "description": "Shown in the plugin's settings", "default": false }]
}
```

The agent runs `command` in the plugin directory. The directory name is the plugin name. Use lowercase letters, digits and dashes.

`author` is optional. It is the author's GitHub login. `/plugins` shows it as `@octocat`.

`commands` is optional. While the plugin runs, the TUI shows each command in the `/` menu.

`tools` is optional. While the plugin runs, the model gets each tool as `<plugin>__<name>`, for example `my-plugin__lookup`. A tool name has letters, digits, `_` and `-`, and no `__`. Without `input_schema`, the tool takes no input. The user approves plugin tools like other tools that change state. Plan mode blocks them.

`settings` is optional. Each entry has a `key` and a `default`, and an optional `description`. The default is a boolean or a string. In `/plugins`, Enter opens the plugin's settings. Space or Enter toggles a boolean or edits a string. The values are kept in `plugin_settings` in `~/.agents/settings.json`. A value with a type that is not the type of the default is ignored.

## Messages from the agent

Every message has a `context`:

```json
{ "provider": "anthropic", "model": "claude-opus-5-5", "accountId": "…", "accountName": "work", "cwd": "/repo", "sessionId": "…", "settings": { "verbose": false } }
```

`settings` holds the value of each setting in `plugin.json`, with the user's changes.

`accountId` is the Claude account uuid or the ChatGPT account id. It is `null` for API keys. `accountName` is the name the user gave the account at `/login`. It is `null` when the provider has no account.

`provider`, `accountId` and `accountName` are `null` when every model list failed at startup.

`event` is a notification. Do not answer it. `params.type` is one of:

| type | when | other params |
| --- | --- | --- |
| `session_start` | a session starts, resumes or is cleared | |
| `tool_call` | before the model's tool call runs | `name`, `input` |
| `tool_result` | after the tool call | `name`, `output`, `isError` |
| `prompt_end` | the agent finishes a prompt | `error` (string or null) |
| `account_change` | the user picks the session's account in `/account` | |
| `account_replace` | `/login` overwrites a saved account with one that has another account id | `previousAccountId`, `accountId` |
| `panel_select` | the user presses Enter on a row of the plugin's panel | `panel` (the title), `id` (the row's `id`) |

`input` is a request with `params.text` and `params.context`. The agent sends it before each prompt. Answer with one of these results:

```json
{ "action": "continue" }
{ "action": "transform", "text": "the new prompt" }
{ "action": "block", "reason": "Shown to the user instead of running the prompt." }
```

The agent asks plugins in name order. Each plugin gets the text that the plugins before it returned. If a plugin does not answer in 5 seconds, sends an error, or exits, the prompt continues.

`command` is a request with `params.name`, `params.args` and `params.context`. The agent sends it when the user types `/name args`. Answer with `{ "text": "Shown to the user." }`, or send an error with a `message`. The agent waits 5 seconds. Commands do not go to the model, also during a turn.

`tool` is a request with `params.name` (without the plugin prefix), `params.input` and `params.context`. The agent sends it when the model calls one of the plugin's tools. Answer with `{ "text": "Shown to the model.", "isError": false }`, or send an error with a `message`. There is no time limit. If the user presses Esc, the agent stops waiting and the model gets `Operation aborted`. The plugin gets no message about this. The agent ignores an answer that comes later, so a plugin that does long work must stop it by itself.

## Messages to the agent

`ui.setStatus` is a notification with `params.text`. The agent shows the text in the footer. Send `null` to remove it. To color parts of the text, send an array of parts, for example `[{ "text": "5h " }, { "text": "92%", "tone": "error" }]`. `tone` is `ok` (green), `warn` (orange) or `error` (red). A part without a tone is muted.

`ui.setAccountLabels` is a notification with `params.labels`, an object from `accountId` to text. The `/account` panel shows the text in a column named after the plugin, on the row of the account with that id. Each message replaces the plugin's earlier labels.

`ui.showPanel` is a notification that opens a panel in the TUI:

```json
{ "title": "Fleet", "rows": [{ "id": "a", "left": "acct-1", "right": "80%" }], "hint": "enter switch" }
```

When the user presses Enter on a row, the plugin gets a `panel_select` event. Send `ui.showPanel` again to update the panel. The selected row stays the same. Send `null` as params to close the panel. The panel does not open if the user is in a different panel.

The agent answers other requests with error `-32601`.
