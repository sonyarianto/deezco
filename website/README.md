# Deezco Website (VitePress)

Official docs for [Deezco](https://github.com/sonyarianto/deezco) — lives in `website/` at the repo root.

## Dev

```bash
cd website
npm install
npm run docs:dev     # http://localhost:5173
```

## Build

```bash
npm run docs:build   # -> .vitepress/dist
npm run docs:preview
```

## Deploy to Vercel (manual)

1. Vercel Dashboard → Add New Project → Import `sonyarianto/deezco`.
2. **Root Directory** = `website`
3. **Build Command** = `npm run docs:build`
4. **Output Directory** = `.vitepress/dist`
5. **Install Command** = `npm install`
6. Deploy (manual — no auto-deploy required).

Or via CLI:

```bash
npm i -g vercel
cd website
vercel --prod
```

`website/vercel.json` already sets `buildCommand`/`outputDirectory` for this layout.
