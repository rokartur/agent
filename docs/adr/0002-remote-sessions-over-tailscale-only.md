---
status: proposed
---

# Remote sessions reach other computers over Tailscale only, with no cloud relay

A session server accepts remote clients only on its tailnet address, over plain TCP, and admits a peer only when `tailscale whois` reports the host's own login. Unlike t3code, which this design follows otherwise, there is no hosted relay, account, pairing token, TLS or WebSocket: WireGuard already encrypts the link and Tailscale already proves who the peer is, so each of those would add a dependency or a service to run without adding protection for a single user's computers. The cost: no access from outside the tailnet, no browser or phone client without more work, and revoking a computer means removing it from the tailnet. See #60 and #62.

## Considered options

- A cloud relay like T3 Connect: reachable from anywhere, but a service to host and trust.
- Pairing tokens on top of Tailscale: per-device revocation, but a second identity system for the same set of computers.
- TLS and WebSocket: needed only by a browser client, which is out of scope.
