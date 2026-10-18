# Contributing to OpenApp

Thanks for helping. A few rules keep the project coherent:

- **The kernel ships primitives, not features.** If something can be built by the in-app agent as a module, it does
  not belong in the kernel. New kernel work adds a building block (see `src-tauri/src/primitives/README.md`) with a
  consent class, a `describe` entry and tests.
- **Rust for the kernel, TypeScript for everything else.** No new JavaScript source files.
- **Contracts first.** Behaviour changes update `spec/CONTRACT.md` in the same change.
- **Keep the agent honest.** Changes to the builder skill (`workspace-template/skills/openapp-builder/`) keep the core
  short and are checked against the probes in `eval/`.

## Development

```bash
pnpm install
node scripts/fetch-runtime.ts
pnpm tauri dev
```

Tests: `pnpm test` (lint + unit + kernel suites), `pnpm test -- --vm` (VM suite), `pnpm test -- --gui` (shell suite).
See [`TESTING.md`](TESTING.md).

## Security

Please report vulnerabilities privately through GitHub's "Report a vulnerability" on the repository's Security tab,
not in public issues.
