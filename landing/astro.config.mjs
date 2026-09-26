// @ts-check
import { defineConfig } from 'astro/config'
import sitemap from '@astrojs/sitemap'

// Custom-domain deploys are served from the domain root, so keep Astro's base
// path at "/" and let absolute SEO URLs be configured by the deploy env.
export default defineConfig({
  site: process.env.PUBLIC_SITE_URL ?? 'https://syrtis.nyanako.com',
  trailingSlash: 'ignore',
  integrations: [
    sitemap({
      // emit xhtml:link hreflang alternates inside the sitemap as well
      i18n: {
        defaultLocale: 'en',
        locales: { en: 'en', 'zh-tw': 'zh-Hant-TW' },
      },
    }),
  ],
})
