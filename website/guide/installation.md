# Installation

## Requirements

- Rust **1.88+** (edition 2024)
- A valid Deezer ARL cookie (see [Authentication](/guide/authentication))

## From source

```bash
git clone https://github.com/sonyarianto/deezco.git
cd deezco
cargo build --release
```

Binary at `target/release/deezco`.

```bash
./target/release/deezco --help
./target/release/deezco --version
```

## Output directory

Default: OS Downloads folder (`~/Downloads`). Override with `-o` or `DEEZCO_OUTPUT_DIR`.

```bash
deezco -o ~/Music track 3135556
DEEZCO_OUTPUT_DIR=~/Music deezco favorites
```

---

## Website (VitePress)

This site lives in `website/` inside the repo.

### Local dev

```bash
cd website
npm install
npm run docs:dev   # http://localhost:5173
```

### Build

```bash
npm run docs:build   # -> website/.vitepress/dist
npm run docs:preview # preview the build
```

### Deploy the website to Vercel (manual)

You requested manual deployment to Vercel (no auto CI). Steps:

1. **Push** the repo to GitHub (already at `sonyarianto/deezco`).
2. In **Vercel Dashboard** → **Add New Project** → Import the GitHub repo.
3. **Framework Preset:** VitePress
4. **Root Directory:** `website` (important — the site is not at repo root)
5. **Build & Output Settings:**
   - Build Command: `npm run docs:build`
   - Output Directory: `.vitepress/dist`
   - Install Command: `npm install`
6. **Deploy.** Vercel will run the build inside `website/` and publish the static output.
7. For future updates you redeploy manually from Vercel (or click **Redeploy**).

Alternative via Vercel CLI:

```bash
npm i -g vercel
cd website
vercel          # first time: link project, set root to website
vercel --prod   # manual prod deploy
```

`website/vercel.json` is already configured so `vercel` auto-detects the correct build/output paths even if you keep Root Directory at repo root.

### Custom domain (optional)

Vercel → Project → Settings → Domains → Add domain. No env vars needed for the docs site.
