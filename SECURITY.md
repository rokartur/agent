# Security policy

## Supported versions

Only the latest release gets security fixes. The agent updates itself, so most users run the latest release.

## Updates and downloads

Releases are built by GitHub Actions when a `v*` tag is pushed. The updater and `install.sh` download the binary from the GitHub release over HTTPS and compare it with the `SHA256SUMS` file of the same release. This check finds a damaged or incomplete download. It does not protect against a release that was replaced on GitHub, because the checksum comes from the same place as the binary. Releases are not signed.

## Report a vulnerability

Do not open a public issue for a security problem.

Report it privately on GitHub: go to the [Security tab](https://github.com/rokartur/agent/security/advisories/new) and click **Report a vulnerability**.

Include:

- the affected version (`agent --version`) and OS,
- the steps or a proof of concept that show the problem,
- the impact, for example which data an attacker can read or which command they can run.

You will get a reply within 7 days. After the fix is released, the advisory will be published. If you want credit, the advisory will name you.

## Scope

These are in scope:

- a way to run a tool or a command without the approval that the permission mode requires,
- a leak of API keys, OAuth tokens or other data from `~/.agents`,
- a plugin, MCP server, web page or file that makes the agent act outside its permission mode.

A model that follows the prompt you gave it, in a mode that you approved (for example `--yolo`), is not a vulnerability.
