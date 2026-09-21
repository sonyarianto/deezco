# Serving

`serve` exposes playlists over HTTP for players that pull track-by-track (e.g. crabsoup) and includes a built-in single-file web player.

```bash
deezco serve
deezco serve --host 0.0.0.0 --port 9002
```

Default: `http://127.0.0.1:9001`.

## Endpoints

| Method | Path | Description |
|---|---|---|
| `GET` | `/` | Web player (single-file HTML) |
| `GET` | `/api/health` | `{"status":"ok","crabsoup_compatible":true}` |
| `GET` | `/playlists/{id}` | List tracks |
| `GET` | `/playlists/{id}/next` | Next track (shuffled, no-repeat guard) |
| `POST` | `/playlists/{id}/next` | Same as GET (control-plane friendly) |
| `GET` | `/tracks/{id}` | Fetch audio (`-o track.mp3`) |

```bash
curl http://127.0.0.1:9001/playlists/908622995/next
curl http://127.0.0.1:9001/playlists/908622995
curl http://127.0.0.1:9001/tracks/3135556 -o track.mp3
curl http://127.0.0.1:9001/api/health
xdg-open http://127.0.0.1:9001/
```

Each playlist is fetched once, shuffled, and walked in order. Edits on deezer.com appear within `--refresh-secs` (default 780s):

```bash
deezco serve --refresh-secs 60
```
