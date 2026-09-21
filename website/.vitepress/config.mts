import { defineConfig } from 'vitepress'

export default defineConfig({
  title: 'Deezco',
  description: 'A fast, lightweight Deezer music downloader & Icecast streamer. Zero runtime dependencies.',
  lang: 'en-US',
  base: '/',
  head: [
    ['meta', { name: 'theme-color', content: '#7c3aed' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:title', content: 'Deezco — Deezer downloader & radio streamer' }],
    ['meta', { property: 'og:description', content: 'Fast, lightweight Deezer downloader + Icecast radio streamer with FLAC/MP3, loudness leveling, crossfade & Stereo Tool.' }],
  ],
  themeConfig: {
    search: { provider: 'local' },
    nav: [
      { text: 'Guide', link: '/guide/introduction' },
      { text: 'Streaming', link: '/guide/streaming' },
      { text: 'CLI Reference', link: '/guide/cli-reference' },
      { text: 'GitHub', link: 'https://github.com/sonyarianto/deezco' },
    ],
    sidebar: [
      {
        text: 'Getting Started',
        items: [
          { text: 'Introduction', link: '/guide/introduction' },
          { text: 'Installation', link: '/guide/installation' },
          { text: 'Authentication', link: '/guide/authentication' },
          { text: 'Quick Start', link: '/guide/quickstart' },
        ]
      },
      {
        text: 'Usage',
        items: [
          { text: 'Downloading', link: '/guide/usage' },
          { text: 'Serving (HTTP)', link: '/guide/serving' },
          { text: 'Streaming (Icecast)', link: '/guide/streaming' },
          { text: 'Output Layout', link: '/guide/output' },
        ]
      },
      {
        text: 'Reference',
        items: [
          { text: 'CLI Reference', link: '/guide/cli-reference' },
          { text: 'Quality & Fallback', link: '/guide/quality' },
          { text: 'Pipeline & DSP', link: '/guide/pipeline' },
        ]
      }
    ],
    socialLinks: [
      { icon: 'github', link: 'https://github.com/sonyarianto/deezco' }
    ],
    footer: {
      message: 'Released under the MIT License.'
    },
    editLink: {
      pattern: 'https://github.com/sonyarianto/deezco/edit/main/website/:path',
      text: 'Edit this page on GitHub'
    },
    lastUpdated: { text: 'Updated at' },
    outline: 'deep',
  },
  vite: {
    server: { port: 5173 }
  }
})
