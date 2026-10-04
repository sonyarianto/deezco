# Deezco Web Control Plane — Design Doc

Status: **Phase 1 shipped, Phase 2 deferred (recorded)**
Date: 2026-08-22
Commits: `5e6a766`, `7c6bc21`

> Note (2026-09-19): code references below use symbol names, not `file:line`
> numbers — line numbers drift on every edit, symbols don't.

## 1. Background

Deezco is currently CLI-only (`Commands` in `src/cli.rs`):
- `deezco serve` — pull model for crabsoup via the router in `src/serve.rs` (`GET /playlists/{id}/next`, `GET /playlists/{id}`, `GET /tracks/{id}`)
- `deezco stream` — push model to Icecast via `icecast::stream` / `Producer` in `src/icecast.rs` (`PUT` + `icy-metaint:16000`)

Pain point: `stream` has no runtime observability/control (requires `Ctrl+C` to change playlist/skip), `serve` needs an external app for playback.

Idea: add a web interface as a **player** (playlist preview) and a **control plane** for Icecast, with the constraint **MUST NOT BREAK the crabsoup contract**.

## 2. Key Design Decisions

1. **Additive, not breaking** — the 3 crabsoup endpoints stay 100% identical (route registration in `serve()` inside `src/serve.rs`). The web only adds routes next to them: `GET /`, `GET /api/health`, `POST /playlists/{id}/next`. Auth (if added later) applies only to `/api/*` & `/`, never to the 3 legacy endpoints.
2. **Single binary stays** — web player is a single-file HTML embed via `src/web_assets.rs` (`include_str!` style), no Vite/React/build step. Binary stays small (~3.2MB when this doc was written; ~3.8MB since the Symphonia decoder landed 2026-09-19 — release profile `opt-level=2, lto, strip` in `Cargo.toml`).
3. **Reuse 90% of infra** — `DeezerApi::get_playlist_tracks` (`src/api.rs`), `TrackQueue::next_track` (`src/queue.rs`), `fetch_track_audio` (via `crate::download`) are called as-is. The web is just an `axum::Router` wrapper (`axum 0.8` is already in `Cargo.toml`).

## 3. Phase 1 — Shipped (2026-08-22)

### Endpoints
- `GET /` → `web_index` (`src/serve.rs`) — vanilla-JS HTML player (`src/web_assets.rs`), `<audio>` + `fetch('/playlists/{id}/next')`. Deep link `?playlist=908622995`.
- `GET /api/health` → `api_health` (`src/serve.rs`) — `{"status":"ok","crabsoup_compatible":true,"endpoints":[...]}`.
- `GET /playlists/{id}/next` — unchanged (crabsoup).
- `POST /playlists/{id}/next` — alias to the same `playlist_next` handler, for control-plane semantics (route `get(...).post(...)` in `serve()`).
- `GET /playlists/{id}` & `GET /tracks/{id}` — unchanged.

### Verification
- `cargo test` 80 passed, `cargo clippy` & `cargo build` clean.
- Manual `curl /`, `curl /api/health`, `curl /playlists/908622995/next` (GET & POST), `curl /playlists/908622995` — all working.

### Docs
- `README.md` section "Serving playlists to media players": mentions the web player at `http://127.0.0.1:9001/`.

## 4. Phase 2 — Icecast Web Control (Deferred, Design Only)

Goal: a new `deezco web` command unifying `serve` + `stream` in one `axum` process, controllable from phone/VPS without SSH.

### Architecture

```
Browser / phone
   |  GET /  (player)  |  POST /api/icecast/*  |  GET /api/icecast/status (SSE)
   v
Axum Router (ServeState + StreamManager)
   |-> TrackQueue (src/queue.rs) — shared between serve & stream
   |-> StreamManager: Arc<Mutex<Option<JoinHandle<Producer>>>> — wraps Producer + TitleUpdater (src/icecast.rs)
   |-> DeezerApi (src/api.rs)
   v
Icecast PUT /mount (run_connection in src/icecast.rs)  |  /tracks/{id} for the player
```

`StreamManager` is stateful and outlives individual Icecast connections (the reconnect loop in `icecast::stream` stays). `Producer::warm_up` still runs before connecting to avoid silence.

### API (planned, additive)

- `POST /api/icecast/start` `{playlist, music_dir, server, mount, password}` → spawn `icecast::stream` as a background task.
- `POST /api/icecast/stop` → abort the handle.
- `POST /api/icecast/skip` → `queue.next_track` + `Producer::load_next_track`.
- `GET /api/icecast/status` → `{connected, advertised_kbps, reconnect_delay, current_title, listeners?}` — `advertised_kbps`/`current_title` from the same `Producer` methods — via broadcast/SSE.
- `POST /api/icecast/switch-playlist` `{playlist}` → update `config.playlist`.

All under `/api/icecast/*`, never touching the 3 crabsoup endpoints.

### CLI (planned)

```
deezco web --host 127.0.0.1 --port 3000 --refresh-secs 300
# serve + icecast control in one binary
# deezco serve & deezco stream stay as-is (backward compat)
```

Option: `--web-password` flag / `DEEZCO_WEB_PASSWORD` env for basic auth, so the ARL never leaks when binding `0.0.0.0`.

### Risks & Mitigations

- **Binary size** — still a single HTML file; Icecast control is Rust handlers only, no frontend framework.
- **Security** — auth only for `/api/icecast/*` & `/`; the ARL is never exposed to the browser (stays server-side in `auth.rs`).
- **State complexity** — `StreamManager` needs `Mutex<Option<Producer>>` so reconnects resume the same track instead of restarting (like `Arc<Mutex<Producer>>` in `icecast::stream`).

## 5. Next Steps (when resumed)

1. Create `src/web.rs` — `WebState { serve_state, stream_manager }`, combined router.
2. Add `Commands::Web` to `Commands` (`src/cli.rs`).
3. Implement the 4 icecast endpoints above + SSE for now-playing.
4. Update the `README.md` "Web Control Plane" section.
5. Test: `cargo test`, `curl` all old + new endpoints, `xdg-open http://127.0.0.1:3000/`.

## 6. File Reference

- `src/serve.rs` — crabsoup router (`serve()`), web player (`web_index`, `api_health`, `playlist_next`)
- `src/web_assets.rs` — embedded Phase 1 web player
- `src/icecast.rs` — `Producer` (`warm_up`, `load_next_track`, `current_title`, `advertised_kbps`), `TitleUpdater`, `icecast::stream` (reconnect loop, `Arc<Mutex<Producer>>`), `run_connection`
- `src/queue.rs` — `TrackQueue::next_track` & stale refresh
- `src/api.rs` — `DeezerApi` playlist & track fetch
- `Cargo.toml` — `axum` dependency, release profile

---
*Recorded per user request 2026-08-22 to defer Phase 2.*
