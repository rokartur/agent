# Contributing

Thank you for your help. Bug reports, fixes and small features are welcome.

## Before you start

- For a bug, open an issue with the steps that show it, the `agent --version` output and your OS.
- For a feature or a large change, open an issue first. The project keeps the agent small, so not every feature goes in.
- For a security problem, do not open an issue. Read [SECURITY.md](SECURITY.md).

## Build and test

You need a stable Rust toolchain. The tests also run `rg` (ripgrep), `fd` and ImageMagick, so these must be on your `PATH`:

- macOS: `brew install ripgrep fd imagemagick`
- Debian or Ubuntu: `sudo apt-get install ripgrep fd-find imagemagick`, then `sudo ln -s "$(command -v fdfind)" /usr/local/bin/fd`
- Windows: `choco install ripgrep fd imagemagick.app`

```sh
cd source
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo fmt --check
```

CI runs these three checks on each pull request, the tests on Linux, macOS and Windows.

## Pull requests

- Keep one change in one pull request.
- Add a test for each new behavior or bug fix, next to the code it tests.
- Use [Conventional Commits](https://www.conventionalcommits.org) for commit messages, for example `fix(tui): keep queued images`.
- Do not add a dependency for something a few lines of code can do.
- Update `README.md` or `plugins/README.md` if the change is visible to users.

## License

The project uses the [GNU General Public License v3.0 or later](LICENSE). When you send a contribution, you agree to license it under the same terms.

## Code of conduct

All contributors must follow the [Code of Conduct](CODE_OF_CONDUCT.md).
