---
layout: home

hero:
  name: "Deezco"
  text: "Deezer downloader & Icecast radio"
  tagline: "Fast, lightweight, zero runtime dependencies. FLAC / MP3 320 / 128 with parallel downloads, Blowfish decryption, and a broadcast-grade Icecast pipeline."
  image:
    src: /logo.svg
    alt: Deezco
  actions:
    - theme: brand
      text: Get Started
      link: /guide/introduction
    - theme: alt
      text: View on GitHub
      link: https://github.com/sonyarianto/deezco
    - theme: alt
      text: CLI Reference
      link: /guide/cli-reference

features:
  - icon: ⬇️
    title: Fast Downloads
    details: "Track, playlist, album, artist discography, favorites & followed artists. Parallel downloads with configurable concurrency and skip-existing."
  - icon: 🎛️
    title: Quality-aware
    details: "FLAC, 320, 128 with automatic fallback, --min-quality / --max-quality / --exact and --dry-run preview of the real format."
  - icon: 📻
    title: Icecast Radio
    details: "Stream any playlist as a 24/7 CBR radio source with ICY metadata, reconnect, prefetch, jingle/silence filler & pacing."
  - icon: 🎚️
    title: Broadcast Pipeline
    details: "Optional PCM bus — R128 loudness (-14 LUFS), equal-power crossfade, static gain, and Thimeo Stereo Tool (CLI or libStereoTool)."
  - icon: 🌐
    title: Serve / Web Player
    details: "Expose playlists over HTTP for crabsoup and a built-in web player at / — plus /api/health. No extra build step."
  - icon: 🔐
    title: Simple Auth
    details: "ARL cookie via --arl, DEEZCO_ARL, stored file or interactive prompt. Login persisted in ~/.config/deezco/.arl."
---

## One binary, everything you need

```bash
# install from source
git clone https://github.com/sonyarianto/deezco.git
cd deezco
cargo build --release
./target/release/deezco --help

# download
deezco track 3135556
deezco -q flac favorites
deezco playlist https://www.deezer.com/en/playlist/908622995

# serve
deezco serve --port 9001
# -> http://127.0.0.1:9001/

# stream to Icecast
deezco stream 908622995 --server http://localhost:8000 --mount /radio --password hackme --crossfade 6 --target-lufs -14
```

<div class="tip custom-block" style="padding-top: 8px">

**Manual deploy to Vercel:** this site lives in `website/` as a VitePress app. See [Deployment](/guide/installation#deploy-the-website-to-vercel-manual) for Vercel steps (import repo → set Root Directory = `website` → Build Command `npm run docs:build` → Output `website/.vitepress/dist`).

</div>

## Why Deezco?

- **Zero runtime deps** for downloads & native streaming; FFmpeg/LAME replaced by bundled Symphonia + LAME MP3 encoder.
- **Predictable quality:** sorted results & JSON output by `quality` by default, not relevance.
- **Radio-grade continuity:** session-persistent CBR encoder — no splice clicks, no per-track re-encode boundaries.
- **Small binary:** `opt-level=z + lto + strip` (< 4 MB).
