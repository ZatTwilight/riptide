# Riptide Remote API

Local HTTP + Server-Sent-Events API for controlling riptide and reading its
now-playing state. Intended for desktop dashboards, phone remotes, and similar
clients on a trusted network.

## Enabling

Set `remote.enabled` to `true` in `~/.config/riptide/config.json` and restart:

```json
"remote": {
  "enabled": true,
  "port": 16385,
  "bind": "127.0.0.1",
  "token": null
}
```

| Key      | Meaning                                                              | Default      |
|----------|----------------------------------------------------------------------|--------------|
| `enabled`| Serve the API                                                        | `false`      |
| `port`   | TCP port                                                             | `16385`      |
| `bind`   | Listen address (`127.0.0.1` = localhost only)                        | `127.0.0.1`  |
| `token`  | Bearer token; auto-generated on first enable. Delete to regenerate   | generated    |

To expose the API to other devices, change `bind` to e.g. `0.0.0.0` or your LAN
IP. Beyond localhost, the token is the only protection — keep it secret.

## Authentication

Every route requires a bearer token when one is configured:

```
Authorization: Bearer <token>
```

Browser `EventSource` cannot set headers, so the token may also be passed as a
query parameter on any route: `GET /events?token=<token>`.

On normal startup, enabling the API with `token` null/absent generates and saves
one before the listener starts. Read it from `config.json`; do not share it.
The API uses plain HTTP, so use it only over a trusted connection.

## Reading state

### `GET /now-playing`

One-shot JSON snapshot of the player:

```json
{
  "active": true,
  "paused": false,
  "track": {
    "id": 382274002,
    "title": "Pretty Low",
    "artists": ["Dillon Francis", "Galantis", "Arden Jones", "Good Humans"],
    "album": "Pretty Low",
    "album_id": 382274001,
    "art_url": "https://resources.tidal.com/images/.../640x640.jpg",
    "duration_secs": 173.0,
    "quality": null,
    "bit_depth": 16,
    "sample_rate": 44100,
    "codec": "Symphonia · FLAC"
  },
  "position_secs": 65.285,
  "volume": 80,
  "shuffle": false,
  "repeat": "one",
  "queue_index": 1,
  "queue_len": 21
}
```

Field notes:

- `active` — playback has started and has not stopped or exhausted the queue.
  The last track may remain present while inactive; `/play` can restart it.
- `art_url` — presentation-size (640×640) cover on Tidal's CDN; fetch it
  directly. Empty string when no art is known.
- `quality` — catalogue badge: `"MAX"`, `"ATMOS"`, `"HI-FI"`, or `null`. What
  the *release* offers.
- `bit_depth` / `sample_rate` / `codec` — what Tidal actually **delivered** for
  this play. Native FLAC headers verify source resolution; `codec` may also
  include the decoder name. See [audio-quality.md](audio-quality.md).
- `repeat` — `"off"`, `"all"`, or `"one"`.

### `GET /events`

Server-Sent Events stream. `Content-Type: text/event-stream`, one JSON event
per state change:

```
data: {"active":true,...}

data: {"active":true,...}

: keepalive
```

- The **first event on connect** is the snapshot current at that moment.
- **Any meaningful change pushes immediately**: track change, pause/play,
  volume, shuffle, repeat, queue changes, delivered quality.
- **Pure playhead advances are throttled to one event per 5 s.** Compute the
  current position client-side:
  `position = event.position_secs + (now − time event was received)`.
- A `: keepalive` comment is emitted every 15 s during silence so proxies
  don't drop idle connections.
- The stream ends when riptide exits. Reconnect (EventSource does this
  automatically); the first event re-syncs you.

## Controlling playback

All transport routes are `POST`, take no body, and answer `204 No Content`.
Errors are JSON: `{"error":"..."}` with a 4xx status.

| Route                       | Effect                                   |
|-----------------------------|------------------------------------------|
| `POST /play`                | Resume / start playing                   |
| `POST /pause`               | Pause                                    |
| `POST /toggle`              | Play if paused, pause if playing         |
| `POST /next`                | Skip to the next queue entry             |
| `POST /previous`            | Back to the previous queue entry         |
| `POST /volume?v=0-100`      | Set volume percent (`400` if `v` is out of range or missing) |

Unknown routes answer `404 {"error":"not found"}`. Missing or wrong
credentials answer `401 {"error":"unauthorized"}`.

## Quick test

```sh
TOKEN=$(jq -r .remote.token ~/.config/riptide/config.json)
BASE=http://localhost:16385

curl -s -H "Authorization: Bearer $TOKEN" $BASE/now-playing | jq
curl -sN -H "Authorization: Bearer $TOKEN" $BASE/events   # -N: no buffering
curl -s -X POST -H "Authorization: Bearer $TOKEN" $BASE/toggle
```
