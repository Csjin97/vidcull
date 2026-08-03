import { sveltekit } from "@sveltejs/kit/vite";
import { svelteTesting } from "@testing-library/svelte/vite";
import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  plugins: [sveltekit(), svelteTesting()],
  test: {
    environment: "jsdom",
    include: ["src/**/*.{test,spec}.{js,ts}"],
    setupFiles: ["./vitest-setup.ts"],
  },
});
