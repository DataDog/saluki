import { defineConfig } from 'vitepress'

export default defineConfig({
  title: "Saluki",
  description: "A toolkit for building telemetry data planes in Rust.",
  base: '/saluki/',
  cleanUrls: true,
  lastUpdated: true,
  themeConfig: {
    search: {
      provider: 'local'
    },

    footer: {
      copyright: 'Copyright © 2024-Present Datadog, Inc.',
    },

    editLink: {
      pattern: 'https://github.com/DataDog/saluki/edit/main/docs/:path',
      text: 'Edit this page on GitHub'
    },
  
    lastUpdated: {
      text: 'Updated at',
      formatOptions: {
        dateStyle: 'full',
        timeStyle: 'medium'
      }
    },

    nav: [
      { text: 'Home', link: '/' },
      { text: 'Developer Guide', link: '/development' }
    ],

    sidebar: [
      {
        text: 'Developer Guide',
        items: [
          { text: 'Common Language', link: '/development/common-language' },
          { text: 'Contributing', link: '/development/contributing' },
          { text: 'Style Guide', link: '/development/style-guide' }
        ]
      },
      {
        text: 'Reference Docs',
        items: [
          { text: 'Architecture', link: '/reference/architecture' }
        ]
      },
      {
        text: 'Agent Data Plane',
        items: [
          { text: 'Releasing', link: '/agent-data-plane/releasing' }
        ]
      }
    ],

    outline: {
      level: [2, 3]
    },

    socialLinks: [
      { icon: 'github', link: 'https://github.com/DataDog/saluki' }
    ]
  }
})
