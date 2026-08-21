# Deezco Web Control Plane — Design Doc

Status: **Phase 1 shipped, Phase 2 deferred (recorded)**
Date: 2026-08-22
Commits: `5e6a766`, `7c6bc21`

## 1. Latar Belakang

Deezco saat ini CLI-only (`src/cli.rs:149`, `src/cli.rs:162`):
- `deezco serve` — pull model untuk crabsoup via `src/serve.rs:158` (`GET /playlists/{id}/next`, `GET /playlists/{id}`, `GET /tracks/{id}`)
- `deezco stream` — push model ke Icecast via `src/icecast.rs:749` (`PUT` + `icy-metaint:16000` + `Producer`)

Pain point: `stream` tidak punya observability/control runtime (harus `Ctrl+C` untuk ganti playlist/skip), `serve` butuh app eksternal untuk playback.

Ide: tambah web interface sebagai **player** (preview playlist) dan **control plane** untuk Icecast, dengan constraint **MUST NOT BREAK crabsoup contract**.

## 2. Keputusan Desain Utama

1. **Additive, bukan breaking** — 3 endpoint crabsoup dibiarkan 100% sama (`src/serve.rs:180-184`). Web hanya nambah route di sebelahnya: `GET /`, `GET /api/health`, `POST /playlists/{id}/next`. Auth (jika ada nanti) hanya untuk `/api/*` & `/`, bukan untuk 3 endpoint legacy.
2. **Single binary tetap** — Web player single-file HTML embed via `src/web_assets.rs:1` (`include_str!` style), tanpa Vite/React/build step. Binary tetap ~3.2MB (`Cargo.toml:30` `opt-level=z, lto, strip`).
3. **Reuse 90% infra** — `DeezerApi::get_playlist_tracks` (`src/api.rs:259`), `TrackQueue::next_track` (`src/queue.rs:49`), `download::fetch_track_audio` (`src/serve.rs:128`) tinggal dipanggil. Web cuma wrapper `axum::Router` (sudah ada `axum 0.8` di `Cargo.toml:21`).

## 3. Phase 1 — Shipped (2026-08-22)

### Endpoints
- `GET /` → `web_index` (`src/serve.rs:63`) — HTML player vanilla JS (`src/web_assets.rs:1`), `<audio>` + `fetch('/playlists/{id}/next')`. Deep link `?playlist=908622995`.
- `GET /api/health` → `api_health` (`src/serve.rs:67`) — `{"status":"ok","crabsoup_compatible":true,"endpoints":[...]}`.
- `GET /playlists/{id}/next` — tetap (crabsoup).
- `POST /playlists/{id}/next` — alias sama handler `playlist_next`, untuk semantics control-plane (`src/serve.rs:183` `get(...).post(...)`).
- `GET /playlists/{id}` & `GET /tracks/{id}` — tetap.

### Verifikasi
- `cargo test` 80 passed, `cargo clippy` & `cargo build` clean.
- `curl /`, `curl /api/health`, `curl /playlists/908622995/next` (GET & POST), `curl /playlists/908622995` manual — semua jalan.

### Docs
- `README.md:226` update: mention web player di `http://127.0.0.1:9001/`.

## 4. Phase 2 — Icecast Web Control (Deferred, Design Only)

Tujuan: `deezco web` (command baru) yang menyatukan `serve` + `stream` dalam 1 proses `axum`, sehingga bisa dikontrol dari HP/VPS tanpa SSH.

### Arsitektur

```
Browser / HP
   |  GET /  (player)  |  POST /api/icecast/*  |  GET /api/icecast/status (SSE)
   v
Axum Router (ServeState + StreamManager)
   |-> TrackQueue (src/queue.rs:36) — share antara serve & stream
   |-> StreamManager: Arc<Mutex<Option<JoinHandle<Producer>>>> — bungkus Producer (src/icecast.rs:433) + TitleUpdater (src/icecast.rs:66)
   |-> DeezerApi (src/api.rs:15)
   v
Icecast PUT /mount (src/icecast.rs:658)  |  /tracks/{id} untuk player
```

`StreamManager` = stateful, outlives individual Icecast connections (reconnect logic `src/icecast.rs:766` tetap). `warm_up` (`src/icecast.rs:482`) tetap sebelum connect agar tidak silent.

### API (rencana, additive)

- `POST /api/icecast/start` `{playlist, server, mount, password, bitrate, stereo_tool?}` → spawn `icecast::stream` di background task.
- `POST /api/icecast/stop` → abort handle.
- `POST /api/icecast/skip` → `queue.next_track` + `Producer::load_next_track` (`src/icecast.rs:491`).
- `GET /api/icecast/status` → `{connected, advertised_kbps (src/icecast.rs:598), reconnect_delay, current_title (src/icecast.rs:540), listeners?}` — via broadcast/SSE.
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
- **State complexity** — `StreamManager` perlu `Mutex<Option<Producer>>` agar reconnect resume track yang sama, bukan restart (mirip `Arc<Mutex<Producer>>` di `src/icecast.rs:754`).

## 5. Next Steps (saat dilanjut)

1. Buat `src/web.rs` — `WebState { serve_state, stream_manager }`, router gabungan.
2. Tambah `Commands::Web` di `src/cli.rs:149`.
3. Implement 4 endpoint icecast di atas + SSE untuk now-playing.
4. Update `README.md` section "Web Control Plane".
5. Test: `cargo test`, `curl` semua endpoint lama + baru, `xdg-open http://127.0.0.1:3000/`.

## 6. Referensi File

- `src/serve.rs:158` — router crabsoup
- `src/serve.rs:63`, `src/serve.rs:67`, `src/web_assets.rs:1` — web player Phase 1
- `src/icecast.rs:433`, `src/icecast.rs:540`, `src/icecast.rs:598`, `src/icecast.rs:749` — Producer & stream loop
- `src/queue.rs:36`, `src/queue.rs:49`, `src/queue.rs:62` — queue & stale refresh
- `src/api.rs:259`, `src/api.rs:236` — playlist & track fetch
- `Cargo.toml:21` — axum, `Cargo.toml:30` — release profile

---
*Dicatat atas request user 2026-08-22 untuk Phase 2 nanti dulu.*
