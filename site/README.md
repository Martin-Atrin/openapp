# OpenApp site

Vite + React 19 + TypeScript + Tailwind CSS v4, with Aceternity UI components in `src/components/ui/`.
- Dev: `pnpm --dir site install && pnpm --dir site dev --port 4173` (from the repo root).
- Build: `pnpm --dir site build` writes the static site to `site/dist` (relative `base`, so it works on a project Pages URL).
- Deploy: `.github/workflows/pages.yml` builds and publishes `site/dist` on pushes to `main`.
