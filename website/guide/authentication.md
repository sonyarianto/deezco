# Authentication

Deezco uses Deezer's `arl` cookie.

## Obtaining the ARL

1. Log in at [deezer.com](https://www.deezer.com)
2. Open DevTools (`F12`) → **Application** → **Cookies** → `https://www.deezer.com`
3. Copy the value of the `arl` cookie

## Providing the ARL (priority order)

| Priority | Method | Example | Persisted? |
|---|---|---|---|
| 1 | `--arl` flag | `deezco --arl 123abc track 3135556` | Yes → `~/.config/deezco/.arl` |
| 2 | `DEEZCO_ARL` env | `DEEZCO_ARL=123abc deezco track 3135556` | No (transient, for CI) |
| 3 | Stored file | `~/.config/deezco/.arl` (auto) | Reused across runs |
| 4 | Interactive prompt | stdin | Saved on success |

First run without an ARL prompts on stdin.

## Useful commands

```bash
deezco login                      # verify/persist via prompt (or --arl / DEEZCO_ARL)
deezco --arl 123abc login         # save with flag
deezco --arl 123abc               # also persists and exits (no download)
deezco --show-arl                 # masked: 1234****cdef
deezco --show-arl --reveal        # full ARL (cat ~/.config/deezco/.arl)
deezco logout                     # remove stored ARL
deezco --show-defaults            # inspect effective defaults
deezco --show-defaults --json     # same as JSON
```

`--arl` wins over `DEEZCO_ARL`, which wins over the stored file. On an invalid/expired ARL the file is auto-removed and you are prompted again.
