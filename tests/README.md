# Browser regression tests

Run `pnpm test` from the repository root after installing dependencies and Chromium with `pnpm exec playwright install chromium`. The Playwright configuration starts and stops an isolated Vite server.

`tests/dialog/` mounts the real dedicated-server page and navbar. Its Tauri IPC mock supplies external responses and rejects unexpected commands. Browser tests verify UI requests and state transitions, not remote deployment behavior. The worker-enabled Rust suite covers the deployment, transport, storage, and service boundaries.

The harness uses no live game server, DatHost account, or saved credentials.
