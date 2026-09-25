# Regression checks

Run these commands from the repository root:

```sh
pnpm install
pnpm locale:compile
pnpm exec playwright install chromium
pnpm test
pnpm check
cargo test --manifest-path src-tauri/Cargo.toml --features worker --lib
```

`pnpm test` starts and stops an isolated Vite server and Chromium. It renders the real dedicated-server page (`/tests/dialog/` mounts `src/lib/components/server/ServerPage.svelte`) and shared controls, with Tauri IPC as the external boundary. Deferred responses exercise preview, deployment, config-policy saves, and settings saves. `?saved=1` makes the credential-presence mock report saved secrets; `?mode=local|worker|hosted`, `?status=upToDate|pending|never`, `?nopub=1` (never-published profile), `?failStatus=1` (sync-status calls throw), `?holdStatus=1` (hold sync-status responses until `window.release()`), `?local=1`, `?running=1`, `?unset=1`, `?many=1`, `?restart=1`, `?uploads=N`, `?unchanged=N`, `?unmanaged=N`, `?profile=name`, `?deployed=1` (worker reports a deployed revision), `?wpending=1` (worker reports a pending revision), and `?cancelStop=1` steer the fixtures; `window.switchProfile(id)` activates the second mocked profile; `window.fail(cmd)`/`unfail(cmd)` toggle per-command failures; `window.failWorkerRefresh(on)` makes worker-mode live refreshes return a degraded worker-less status with a warning; `window.setWorkerPending`/`setWorkerDeployed` mutate the worker journal between polls. `?component=launch` mounts the launch dropdown, `?component=navbar` mounts the app navbar (with `?nopage=1` to omit the page, `?path=` for the active route, and `window.showServerPage()` to mount it later). Unexpected IPC commands fail the tests. No game server or saved credentials are used.

The Rust suite includes real loopback HTTP, FTP, and FTPS clients, isolated journals, and Windows ACL checks. FTP fixtures own their listeners and connection threads. Restart-policy tests use the real DatHost adapter against a controlled HTTP server. Service lifecycle tests capture the SCM status sink; they do not install a Windows service. Cache-loader and serialized-cache assertions cover legacy metadata separately from the worker initialization gates; they do not simulate a release desktop cache during worker startup. Desktop process `kill`/`watch` and live SFTP/DatHost behavior remain outside this suite's integration coverage.

CI runs the browser checks and the worker-enabled Rust suite on pull requests targeting `master` or `main`. The Windows job includes ACL and service coverage. The existing populated-profile end-to-end test remains opt-in.

## Live FTP diagnostic

The old ignored FTP probe is an explicit diagnostic, not a regression test:

```sh
cargo run --manifest-path src-tauri/Cargo.toml --features diagnostics --example ftp-probe
```

Without `GALE_FTP_PROBE=1`, the command exits before accessing credentials or connecting. To opt in, set `GALE_PROBE_PROFILE_ID`, `GALE_PROBE_HOST`, and `GALE_PROBE_USER`; optional settings are `GALE_PROBE_PORT`, `GALE_PROBE_DIR`, and `GALE_PROBE_BASE`. It reads the selected profile's saved FTP credential from the keyring and prints protocol observations. Probe errors are diagnostic output, not passing regression assertions.

## Lint baseline

The test suite runs without exclusions for the previous NaN equality failure. Its parser regression now asserts NaN explicitly before comparing the remaining document fields.

`cargo clippy --all-targets --features worker -- -D warnings` currently reports pre-existing findings in untouched code, including explicit lifetimes in `config/commands.rs`, nested conditionals, and test fixture style warnings. These are not suppressed by this remediation. The test jobs run independently of the lint job.

Repository-wide `pnpm lint` also reports existing formatting differences outside these changes. Changed frontend test and CI files pass a focused Prettier check. Browser tests run before the frontend lint step.
