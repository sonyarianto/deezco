# Quick Start

```bash
deezco                          # prints help
deezco track 3135556            # download single track by ID/URL
deezco track "Get Lucky"        # search (prints IDs + [FLAC]/duration)
deezco --pick 1 track "Get Lucky"
```

## Common tasks

```bash
# playlist
deezco playlist https://www.deezer.com/en/playlist/908622995
deezco --json playlist 908622995 | jq '.tracks | length'

# favorites
deezco -q flac favorites
deezco --json favorites | jq -r '.tracks[].title'

# artist
deezco artist "Daft Punk"       # prints IDs
deezco artist 27                # downloads full discography

# album
deezco album 302127

# followed artists
deezco following                # all releases from every followed artist
deezco --json following | jq -r '.artists[].name'
deezco --pick 2 following
```

## Quality

```bash
deezco -q flac track 3135556
deezco --min-quality flac track 3135556          # fail instead of fallback
deezco -q flac --max-quality 128 favorites       # cap at 128
deezco --exact flac track 3135556                # min==max shorthand
deezco --dry-run -q flac album 302127            # preview what would be downloaded
```

## Previews

```bash
deezco --preview track 3135556                   # 30s sample
deezco --preview-and-full track 3135556          # both preview + full
```

## JSON & sorting

```bash
deezco --json track "Get Lucky"
deezco --sort duration -l 5 track "Get Lucky"
deezco --sort duration --sort-dir desc -l 5 track "Get Lucky"
deezco --sort quality --json favorites
```
