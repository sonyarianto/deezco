# Deezco Web Control Plane — Design Doc

Status: **Phase 1 shipped, Phase 2 deferred (recorded)**
Date: 2026-08-22
Commits: `5e6a766`, `7c6bc21`

> Note (2026-09-19): code references below use symbol names, not `file:line`
> numbers — line numbers drift on every edit, symbols don't.

## 1. Latar Belakang

Deezco saat ini CLI-only (`Commands` di `src/cli.rs`):
- `deezco serve` — pull model untuk crabsoup via router di `src/serve.rs` (`GET /playlists/{id}/next`, `GET /playlists/{id}`, `GET /tracks/{id}`)
- `deezco stream` — push model ke Icecast via `icecast::stream` / `Producer` di `src/icecast.rs` (`PUT` + `icy-metaint:16000`)

Pain point: `stream` tidak punya observability/control runtime (harus `Ctrl+C` untuk ganti playlist/skip), `serve` butuh app eksternal untuk playback.

Ide: tambah web interface sebagai **player** (preview playlist) dan **control plane** untuk Icecast, dengan constraint **MUST NOT BREAK crabsoup contract**.

## 2. Keputusan Desain Utama

1. **Additive, bukan breaking** — 3 endpoint crabsoup dibiarkan 100% sama (route registration di `serve()` dalam `src/serve.rs`). Web hanya nambah route di sebelahnya: `GET /`, `GET /api/health`, `POST /playlists/{id}/next`. Auth (jika ada nanti) hanya untuk `/api/*` & `/`, bukan untuk 3 endpoint legacy.
2. **Single binary tetap** — Web player single-file HTML embed via `src/web_assets.rs` (`include_str!` style), tanpa Vite/React/build step. Binary tetap kecil (~3.2MB saat doc ini ditulis; ~3.8MB sejak decoder Symphonia masuk 2026-09-19 — release profile `opt-level=z, lto, strip` di `Cargo.toml` tetap).
3. **Reuse 90% infra** — `DeezerApi::get_playlist_tracks` (`src/api.rs`), `TrackQueue::next_track` (`src/queue.rs`), `fetch_track_audio` (via `crate::download`) tinggal dipanggil. Web cuma wrapper `axum::Router` (sudah ada `axum 0.8` di `Cargo.toml`).

## 3. Phase 1 — Shipped (2026-08-22)

### Endpoints
- `GET /` → `web_index` (`src/serve.rs`) — HTML player vanilla JS (`src/web_assets.rs`), `<audio>` + `fetch('/playlists/{id}/next')`. Deep link `?playlist=908622995`.
- `GET /api/health` → `api_health` (`src/serve.rs`) — `{"status":"ok","crabsoup_compatible":true,"endpoints":[...]}`.
- `GET /playlists/{id}/next` — tetap (crabsoup).
- `POST /playlists/{id}/next` — alias sama handler `playlist_next`, untuk semantics control-plane (route `get(...).post(...)` di `serve()`).
- `GET /playlists/{id}` & `GET /tracks/{id}` — tetap.

### Verifikasi
- `cargo test` 80 passed, `cargo clippy` & `cargo build` clean.
- `curl /`, `curl /api/health`, `curl /playlists/908622995/next` (GET & POST), `curl /playlists/908622995` manual — semua jalan.

### Docs
- `README.md` section "Serving playlists to media players": mention web player di `http://127.0.0.1:9001/`.

## 4. Phase 2 — Icecast Web Control (Deferred, Design Only)

Tujuan: `deezco web` (command baru) yang menyatukan `serve` + `stream` dalam 1 proses `axum`, sehingga bisa dikontrol dari HP/VPS tanpa SSH.

### Arsitektur

```
Browser / HP
   |  GET /  (player)  |  POST /api/icecast/*  |  GET /api/icecast/status (SSE)
   v
Axum Router (ServeState + StreamManager)
   |-> TrackQueue (src/queue.rs) — share antara serve & stream
   |-> StreamManager: Arc<Mutex<Option<JoinHandle<Producer>>>> — bungkus Producer + TitleUpdater (src/icecast.rs)
   |-> DeezerApi (src/api.rs)
   v
Icecast PUT /mount (run_connection di src/icecast.rs)  |  /tracks/{id} untuk player
```

`StreamManager` = stateful, outlives individual Icecast connections (reconnect loop di `icecast::stream` tetap). `Producer::warm_up` tetap sebelum connect agar tidak silent.

### API (rencana, additive)

- `POST /api/icecast/start` `{playlist, music_dir, server, mount, password}` → spawn `icecast::stream` di background task.
- `POST /api/icecast/stop` → abort handle.
- `POST /api/icecast/skip` → `queue.next_track` + `Producer::load_next_track`.
- `GET /api/icecast/status` → `{connected, advertised_kbps, reconnect_delay, current_title, listeners?}` — `advertised_kbps`/`current_title` dari method `Producer` yang sama — via broadcast/SSE.
- `POST /api/icecast/switch-playlist` `{playlist}` → update `config.playlist`.

Semua di bawah `/api/icecast/*`, tidak sentuh 3 endpoint crabsoup.

### CLI (rencana)

```
deezco web --host 127.0.0.1 --port 3000 --refresh-secs 300
# serve + icecast control dalam 1 binary
# deezco serve & deezco stream tetap ada (backward compat)
```

Opsi: flag `--web-password` / env `DEEZCO_WEB_PASSWORD` untuk basic auth, agar ARL tidak leak saat bind `0.0.0.0`.

### Risiko & Mitigasi

- **Binary size** — tetap single HTML, Icecast control hanya Rust handlers, no frontend framework.
- **Security** — auth hanya untuk `/api/icecast/*` & `/`, ARL tidak di-expose ke browser (tetap server-side `auth.rs`).
- **State complexity** — `StreamManager` perlu `Mutex<Option<Producer>>` agar reconnect resume track yang sama, bukan restart (mirip `Arc<Mutex<Producer>>` di `icecast::stream`).

## 5. Next Steps (saat dilanjut)

1. Buat `src/web.rs` — `WebState { serve_state, stream_manager }`, router gabungan.
2. Tambah `Commands::Web` di `Commands` (`src/cli.rs`).
3. Implement 4 endpoint icecast di atas + SSE untuk now-playing.
4. Update `README.md` section "Web Control Plane".
5. Test: `cargo test`, `curl` semua endpoint lama + baru, `xdg-open http://127.0.0.1:3000/`.

## 6. Referensi File

- `src/serve.rs` — router crabsoup (`serve()`), web player (`web_index`, `api_health`, `playlist_next`)
- `src/web_assets.rs` — embedded web player Phase 1
- `src/icecast.rs` — `Producer` (`warm_up`, `load_next_track`, `current_title`, `advertised_kbps`), `TitleUpdater`, `icecast::stream` (reconnect loop, `Arc<Mutex<Producer>>`), `run_connection`
- `src/queue.rs` — `TrackQueue::next_track` & stale refresh
- `src/api.rs` — `DeezerApi` playlist & track fetch
- `Cargo.toml` — dependensi `axum`, release profile

---
*Dicatat atas request user 2026-08-22 untuk Phase 2 nanti dulu.*
