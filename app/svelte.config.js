import adapter from "@sveltejs/adapter-static";
import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";


const config = {
  preprocess: vitePreprocess({ style: false }),
  kit: {
    adapter: adapter({
      pages: "dist",
      assets: "dist",
      fallback: "200.html",
      precompress: false,
      strict: true,
    }),
  },
};

export default config;
