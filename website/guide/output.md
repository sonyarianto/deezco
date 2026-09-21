# Output Layout

Within the output directory:

- **Track** → `<output>/<Artist>/<Artist> - <Title>.<ext>`
- **Playlist** → `<output>/<Playlist Name>/`
- **Favorites** → `<output>/Favorites/`
- **Artist** → `<output>/<Artist>/<Album>/`
- **Album** → `<output>/<Artist>/<Album>/`

Filenames are sanitized; existing files are skipped (`skip-existing`).

## Quality fallback

Requested `flac`/`320` may fall back to `128` on free accounts. Output shows:

- `[FLAC]` — as requested
- `[MP3_320 — FLAC unavailable]` — fallback
- Native MP3 passthrough keeps the fetched format; pipeline transcodes to session CBR.

Check with `--dry-run`:

```bash
deezco --dry-run -q flac album 302127
```
