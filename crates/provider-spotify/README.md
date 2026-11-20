# Spotify provider

The coordinator submits bounded work for catalogue queries, library operations and playback. The provider owns HTTP requests, rate limiting and the projection of remote state. Session generations discard late responses after an account or source change. Verified events from a locally supervised Soloist can supply commands and playback clocks for that device.

Web API requests use a fixed HTTPS base, bounded responses and request timeouts. The host refreshes expired credentials. Tests use a loopback HTTP server with original metadata and fixture tokens. The account setup and coverage guide describe what requires a live account. [Setup](../../docs/spotify.md) · [API coverage](../../docs/spotify-api-coverage.md)
