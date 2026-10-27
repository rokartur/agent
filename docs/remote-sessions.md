# Remote sessions

A session can run on another of your computers on the same [Tailscale](https://tailscale.com) tailnet. The session uses that computer's files, shell, settings, accounts, MCP servers, plugins and skills. Your terminal only shows it. The connection is plain TCP inside the tailnet: WireGuard encrypts it and Tailscale tells the host who connects. There is no cloud relay and no pairing code. See [ADR 0002](adr/0002-remote-sessions-over-tailscale-only.md).

The examples call the host `studio`. Use its Tailscale name, as `tailscale status` shows it.

## On the host

1. Install agent and log in to your providers there. A remote session uses the host's accounts, not the ones on your laptop.
2. In the host's `~/.agents/settings.json`, turn on the listener:

   ```json
   { "serve_tailnet": true }
   ```

3. Run the listener:

   ```sh
   agent serve --tailnet
   ```

   It listens only on the host's Tailscale addresses, never on `0.0.0.0`, on port 24368 (`tailnet_port` changes it). It accepts a device only when `tailscale whois` reports your own login for it. Tagged devices and devices of other users of a shared tailnet are refused. A computer without `"serve_tailnet": true` opens no network port.

To start the listener at login on macOS, save this as `~/Library/LaunchAgents/com.rokartur.agent.tailnet.plist`, change the paths to yours, and run `launchctl load ~/Library/LaunchAgents/com.rokartur.agent.tailnet.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.rokartur.agent.tailnet</string>
  <key>ProgramArguments</key>
  <array><string>/Users/you/.local/bin/agent</string><string>serve</string><string>--tailnet</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string></dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>/Users/you/Library/Logs/agent-tailnet.log</string>
</dict>
</plist>
```

Sessions get the listener's environment, so put every directory that holds the tools you use (git, rg, compilers) in `PATH`. With the macOS firewall on, allow incoming connections for `agent` when macOS asks.

On Linux, use a systemd user unit, `~/.config/systemd/user/agent-tailnet.service`, then run `systemctl --user enable --now agent-tailnet` and `loginctl enable-linger` so it runs without a login:

```ini
[Unit]
Description=agent sessions over Tailscale
After=network-online.target tailscaled.service

[Service]
ExecStart=%h/.local/bin/agent serve --tailnet
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin
Restart=always
RestartSec=5

[Install]
WantedBy=default.target
```

## On the computer you type on

List the host in `~/.agents/settings.json`:

```json
{ "environments": ["studio"] }
```

Then:

- `agent --on studio` starts a session on `studio`. Flags and a message work as they do locally: `agent --on studio -c`, `agent --on studio fix the failing test`. A piped message does not reach a remote session.
- `"default_environment": "studio"` makes `agent` start every session there. `agent --on local` still starts one here.
- `/env` lists the environments. `/env studio` starts a new session on `studio` in this terminal, and the session you leave keeps running.
- `agent attach` lists the running sessions of every environment, tagged with the computer. A computer that does not answer shows the sessions it had when it last answered, greyed out. `agent attach <id>` finds the session on any of them.

A new remote session starts in the same directory path as your terminal. When that directory does not exist on the host, it starts in the host's home directory and says so. Move it with `!cd <dir>`, then use `/worktree` to pick a branch or a worktree there.

The project is the host's: `@` file completion, `!` commands, git status and diffs all come from the host. What you do on your own computer stays there: Ctrl+V pastes an image from your clipboard, a file you drag into the terminal is read from your disk, and `/copy`, copied selections and links you click use your clipboard and your browser.

When the connection drops (Wi-Fi off, a network change, sleep), the session keeps running on the host, and a permission prompt waits there. Your terminal keeps the session on screen, says it is reconnecting, and tries again every few seconds. Once the host answers, the screen shows everything the session did meanwhile and takes keys again. Keys typed while it reconnects are not sent. Ctrl+C stops trying and leaves; `agent attach` returns to the session later.

Several computers can attach to one session at once, as terminals on one computer can. `/model` lists the host's models. To change the host's settings or log in to a provider there, do it on the host.

The two computers update separately. When their releases cannot talk to each other, the client names the computer to update and the version.
