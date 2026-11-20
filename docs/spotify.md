# Connect Spotify to Auvra

Auvra uses the Spotify Web API for your account and library. Playback can go to an existing Connect device or to this Mac through Spotify’s Soloist runtime. Local files work before you set up either connection. [API coverage](spotify-api-coverage.md)

## Sign in

1. In the Spotify Developer Dashboard, create a Web API app for your own use. Copy its public **Client ID**. The owner of a development-mode app needs Premium. If someone else is testing the same registration, add their account in **Settings → Users Management**. [Developer Dashboard](https://developer.spotify.com/dashboard) · [Development mode and limits](https://developer.spotify.com/documentation/web-api/concepts/quota-modes)
2. Register `http://127.0.0.1:43821/spotify/callback` exactly. Auvra listens on that loopback address during sign-in. Keep the HTTP scheme and explicit port. [Spotify redirect URI rules](https://developer.spotify.com/documentation/web-api/concepts/redirect_uri)
3. Open **Auvra → Settings → Account**. Enter the Client ID in **Spotify app Client ID** and click **Connect Spotify**. Keep Auvra open while you finish in the browser. The app uses PKCE, so you only need the public ID.
4. Return to **Your Library** and open **Liked Songs**. If you signed in with an older set of permissions, use **Update Spotify Permissions** in Account for playlist edits, listening history, personal tops or saved podcast and audiobook progress.

## Play directly on this Mac

1. Download and extract the official **Apple Silicon Mac (arm64)** Soloist archive. In **Settings → Account → Playback on this Mac**, choose **Install Runtime…** and select the extracted `soloist` executable. Auvra checks its signature. [Official downloads and updates](https://developer.spotify.com/documentation/soloist/reference/downloads-and-updates)
2. In the Soloist Dashboard, generate your own personal key. In Auvra, enter it in **Soloist API key** and click **Save Key & Start**. [Soloist Dashboard](https://developer.spotify.com/dashboard/soloist)
3. Open Spotify on the same local network, open its device picker and select **Auvra · …**. The suffix identifies this installation. Allow Auvra local network access if macOS asks. After this first pairing, you can close Spotify and start a track from Auvra. [Spotify’s pairing and distribution instructions](https://developer.spotify.com/documentation/soloist)
4. To switch outputs later, use the **Playback device** menu. Choose **Play on this Mac · Auvra** or an available Connect device. Playback on another device uses that device’s own audio output and queue.

## If something stops working

| Symptom | What to check |
| --- | --- |
| The browser cannot return to Auvra | Keep Auvra open, verify the registered URI character for character, finish or cancel any other sign-in attempt, then start a new one from Account. |
| The Auvra device is missing | Check the runtime status and key in Account. Put both devices on the same LAN, allow Auvra local network access, then reopen Spotify’s device list. |
| Soloist reports that its build expired | Install a fresh Apple Silicon runtime from Spotify using **Install Runtime…**. Spotify sets a lifetime on its builds. [Runtime updates](https://developer.spotify.com/documentation/soloist/reference/downloads-and-updates) |
| Sign-in succeeds but catalog requests fail | Check the app owner’s Premium status, the user allowlist and the permissions shown in Account. Auvra respects Spotify’s retry interval when requests are rate-limited. |
| The connection drops | Auvra retries saved credentials after temporary network failures. If Spotify has revoked the grant, sign in again. Existing Soloist controls can remain available while Auvra can still verify that local player. |

To remove saved login and pairing data, open **Settings → Account → Disconnect**. You can also revoke the app in your Spotify account. [Data and removal](data-and-privacy.md) · [Manage Spotify apps](https://www.spotify.com/account/apps/) · [Source and distribution](public-release.md)
