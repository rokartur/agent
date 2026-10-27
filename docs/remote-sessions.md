# Remote sessions

A session can run on another of your computers on the same [Tailscale](https://tailscale.com) tailnet. The session uses that computer's files, shell, settings, accounts, MCP servers, plugins and skills. Your terminal only shows it. The connection is plain TCP inside the tailnet: WireGuard encrypts it and Tailscale tells the host who connects. There is no cloud relay and no pairing code. See [ADR 0002](adr/0002-remote-sessions-over-tailscale-only.md).

The examples call the host `studio`. Use its Tailscale name, as `tailscale status` shows it.

## On the host

1. Install agent and log in to your providers there. A remote session uses the host's accounts, not the ones on your laptop.
2. In the host's `~/.agents/settings.json`, turn on the listener:

   ```json
   { "serve_tailnet": true }
   ```

3. Start the listener, now and after every login:

   ```sh
   agent service install
   ```

   It listens only on the host's Tailscale addresses, never on `0.0.0.0`, on port 24368 (`tailnet_port` changes it). It accepts a device only when `tailscale whois` reports your own login for it. Tagged devices and devices of other users of a shared tailnet are refused. A computer without `"serve_tailnet": true` opens no network port.

On macOS this loads a launchd agent, `~/Library/LaunchAgents/com.rokartur.agent.tailnet.plist`, which logs to `~/.agents/tailnet.log`; the listener empties that log when it starts and finds it over 1 MB. It starts when you log in, so a Mac without a screen needs automatic login (System Settings > Users & Groups). On Linux it enables a systemd user unit, `~/.config/systemd/user/agent-tailnet.service`, and turns on lingering, so it starts at boot without a login; its log is in `journalctl --user -u agent-tailnet.service`. Either way it starts the listener again when it exits, for example while Tailscale is not up yet.

Sessions get the listener's environment, and `install` gives it the `PATH` of the shell you run it in, so run it where your tools (git, rg, compilers) are found. Run `agent service install` again after you move agent or change `PATH`. With the macOS firewall on, allow incoming connections for `agent` when macOS asks.

- `agent service status` says whether the listener runs and where its log is.
- `agent service restart` starts it again. An update of agent does this itself, so the listener runs the new release.
- `agent service uninstall` stops it and removes it from startup. Sessions that run keep running; `agent attach` on the host reaches them.

To run the listener without the service, for example to watch its output, run `agent service uninstall` first, then `agent serve --tailnet` in a terminal; two listeners cannot share the port.

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
