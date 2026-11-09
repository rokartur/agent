---
status: accepted
---

# Pages are served over HTTP by the session server

The `Page` tool writes the model's HTML into `~/.agents/sessions/<id>/pages/N.html` (the run's private temp folder with `--no-session`, removed on exit) and serves it from the session server through a plain `std::net::TcpListener`, started on the first call. The terminal that typed opens the URL with the `{"open": url}` request clients already handle, or the last attached one when that terminal detached; with no terminal attached, the next one that attaches opens it. With Tailscale up the server listens on the host's tailnet address, so a terminal that attaches later from another device can open the pages too, and admits only untagged devices of the owner through the same `tailscale whois` check as the listener (ADR 0002); without Tailscale, on a tagged host (whose own browser that check refuses) or when the tailnet address cannot be bound, it listens on `127.0.0.1`. Every URL also carries a random 128-bit token and a page number that never repeats in the process, also across `/new` and `/resume`, and the server serves only the files `Page` published, so a path cannot reach anything else. Pages answer back with `agent.submit(data)`, a `POST` to their own URL, accepted only from the page a `wait` call blocks the turn on, until it arrives or the user presses Esc.

Each response has a Content-Security-Policy that allows inline styles and scripts and `data:` images, and connects only to the page's own server. The model writes the HTML, a fetched web page can steer the model, and a page can show code and data from the session: with network access, a page could send that data to another host. CSP cannot stop a navigation the page starts, a WebRTC ICE request or a DNS prefetch, so data can still leave. WebFetch has since been put behind the permission mode for this reason; Page has not, so in `ask` and `auto` mode a page is the one way session data can leave without a prompt.

ADR 0002 left out browser clients. This is a narrow exception: the browser reads pages and posts answers, and it never drives the session.

## Considered options

- Native TUI widgets for charts, forms and cards: no browser, but a widget library on our own renderer, and still poor mocks.
- A component schema in JSON instead of free HTML: a consistent look, but a schema to maintain that is too narrow for mocks. The Arc (uiarc.dev) dark tokens in `document.css`, shared by the stylesheet and the charts, give the consistent look instead.
- Tunnel the pages of a remote session through the session stream to a proxy on the client: no extra port, but an HTTP proxy in the client, and WireGuard already encrypts the direct path.
- Upload to a hosted service such as Postplan: reachable anywhere, but session data would leave the user's computers.
