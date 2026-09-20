# Dedicated servers

Gale can launch a dedicated server installed on the same computer or deploy a published profile to a remote server. Dedicated server support is shown only for games with a `dedicatedServer` entry in `src-tauri/games.json`.

## Using it

Open the dedicated server dialog from the arrow beside the profile launch button.

### Local server

Choose **Local server**, enter the server name, world, port, and optional password, then select **Launch server**. Gale locates the game's dedicated server through its configured platform, prepares the mod loader, and starts the server with the active profile.

The server must already be installed. Gale manages only the process it starts; closing Gale does not silently terminate an unrelated server process.

### Remote server

Choose **Remote server** and enter the connection details supplied by the server host.

- **SFTP (SSH)** supports password and private-key authentication. Confirm the host-key fingerprint the first time Gale connects.
- **FTP / FTPS (automatic)** attempts encrypted explicit FTPS first and falls back to plain FTP only when the host does not support TLS. When a certificate fingerprint is pinned, the connection requires that exact certificate — a different CA-valid certificate does not satisfy the pin. Self-signed certificates require an explicit trust decision before credentials are sent.
- **Server directory** is the directory exposed by the host. Use `/` when the FTP account already opens at the server root.

Use **Test connection** before syncing.

## Selective server synchronization

The server is synchronized from the profile's **canonical publication** — the same revision friends' clients install. The owner publishes once; clients and the dedicated server consume the same artifact.

Open **Sync dedicated server** from the profile's server dialog.

### Scope

- **Mods only** — deploy the published mod payload.
- **Selected configs only** — deploy only the config files you choose.
- **Both** — the default.

### What the planner does

The preview is computed from the live remote listing plus Gale's recorded deployment state:

- Payload files Gale deployed are updated to the published revision and removed when the publication drops them. An owned file whose remote contents were modified is re-uploaded.
- Files Gale has never deployed are **never deleted automatically**. A manually installed `ServerOnly.dll` survives every sync and is reported as _unmanaged_ in the preview.
- World data, saves, logs, host-managed files, and anything outside the managed directories are left alone.
- Package-default configs seed only absent files; they never overwrite a config the server already has.
- Disabling a mod produces `.old` files locally; those are not uploaded.

### Config decisions and policies

Each published config can be applied, declined, or left pending per revision. A deleted-on-server file Gale previously deployed needs an explicit **Restore** decision.

Each config also has a persistent policy — **Ask each update**, **Always apply updates**, or **Always keep my config** — stored in the remote deployment state, so it survives switching between Local and Worker execution. A policy set during revision A only governs future revisions; it does not decide the current conflict.

### Preview and approval

**Preview** computes the exact plan — uploads, removals, config actions, restart need — under the deployment lock. **Deploy** executes it. The approval is bound to the plan hash: if the remote state, the publication, the selection, or the restart policy changes between preview and deploy, the deploy is rejected and a fresh preview is required. The dialog marks the approval stale automatically.

### Restart policy

Deployments that change mod payloads or apply config writes require a server restart to take effect. Choose per deployment:

- **Never (manual)** — deploy only; restart it yourself.
- **Immediately** — restart as soon as the deployment lands.
- **When empty** — restart once no players are connected (requires a hosting provider that reports presence).

No-op, declined, or unselected changes do not trigger a restart.

## Execution modes

### Local

Gale on your PC connects directly to the server and deploys. Nothing else needs to run. Local mode is the default and works for manual deployments.

### Worker

`gale-worker` is a standalone binary that runs on the server host itself (or any always-on machine with access to it). It polls the sync service, deploys approved changes on its own, and can restart the server through the hosting provider's API. Your PC does not need to be online for automatic synchronization.

When to use it: automatic sync, or a host that is only reachable from a fixed location. When the game host (e.g. a game-panel provider) does not allow running arbitrary persistent processes, the worker must live on a separate always-on machine — a VPS, a home server, a NAS — that can reach the game server over FTP/SFTP and the provider's control API. File-management and restart APIs alone do not make a host able to run the worker.

#### Installing the worker

1. Build the binary: `cargo build --release --features worker --bin gale-worker` in `src-tauri`, or copy a provided build to the host.
2. Create `gale-worker.json` (see `src-tauri/src/worker/config.rs` for the full reference):

   ```json
   {
   	"workerId": "my-vps",
   	"listen": "127.0.0.1:8472",
   	"profileId": "the sync profile id",
   	"game": "valheim",
   	"remote": { "protocol": "sftp", "host": "...", "username": "..." },
   	"hostControl": { "provider": "datHost", "datHostServerId": "...", "datHostUsername": "..." },
   	"autoSync": true,
   	"autoMods": true,
   	"restartPolicy": "whenEmpty",
   	"pollIntervalSecs": 300,
   	"stateDir": "/var/lib/gale-worker"
   }
   ```

   `profileId` and `game` pin the worker to one profile — requests cannot redirect it elsewhere.

3. Provide secrets through the environment, never the config file:
   - `GALE_WORKER_TOKEN` — bearer token the API requires.
   - `GALE_WORKER_REMOTE_PASSWORD` — FTP/SFTP password or key passphrase.
   - `GALE_WORKER_REFRESH_TOKEN` — initial sync refresh token; rotated tokens are persisted in the worker's state journal afterwards.
   - `GALE_WORKER_DATHOST_PASSWORD` — DatHost API password, when applicable.

4. Run it persistently, for example as a systemd service:

   ```ini
   [Unit]
   Description=Gale dedicated-server sync worker
   After=network-online.target

   [Service]
   ExecStart=/usr/local/bin/gale-worker --config /etc/gale-worker/gale-worker.json
   EnvironmentFile=/etc/gale-worker/secrets.env
   WorkingDirectory=/var/lib/gale-worker
   Restart=on-failure
   RestartSec=10

   [Install]
   WantedBy=multi-user.target
   ```

   The state journal (`gale-worker-state.json` in `stateDir`) records pending work, retry backoff, and rotated credentials. On POSIX it is created owner-only; keep `stateDir` out of shared directories. A worker restart resumes pending deployments instead of dropping them.

#### Connecting Gale to the worker

In the remote settings, choose **Worker** sync mode and enter the worker's address and token.

- `http://` is accepted **only** for a worker on the same machine (loopback). The bearer token travels in the clear over plaintext HTTP, so remote workers need `https://` — terminate TLS with a reverse proxy (Caddy, nginx, Traefik) or reach the worker through a secure tunnel (WireGuard, Tailscale, SSH port-forward). Do not expose the plaintext API to a LAN and assume it is trusted.
- **Test connection** verifies reachability, the token, and that the worker's bound profile matches this profile.

Manual **Deploy** through the worker works regardless of the `autoSync` toggle. Automation toggles (`autoSync`, `autoMods`) and the restart policy can be changed from the dialog and persist across worker restarts.

## Deployment coordination

Concurrent deployers coordinate through a lease directory claimed atomically on the remote (`/.gale-deploy.lock`). The holder heartbeats its lease while working; release and heartbeat both verify the record still belongs to that operation, so a stale executor cannot delete a successor's lease.

Guarantees:

- Two executors cannot hold the lease at once — the second is told the deployment is busy.
- A crashed executor's lease expires; once the heartbeat is stale past the TTL, a new executor may take it over — the UI surfaces this and offers an explicit takeover. `force` never displaces a live lease.
- An executor that loses the lease stops mutating; state writes carry the operation sequence, so a stale writer cannot overwrite newer state.
- Config-policy writes run under the same lease as deployments.

Limitations to be aware of: FTP/SFTP provide no true fencing — an already in-flight write from a lost-lease executor can still land. The conservative answer is built in: never auto-break a lease that might still be live. If a lease is stuck because an executor hung, confirm it is actually stopped (check the machine or the worker status), then use the takeover affordance in the sync dialog. Treat takeover as the documented recovery path, not routine automation.

## What is preserved

The deployment state file (`.gale-server-state.json` in the server directory, or under `config/` in restricted-root layouts) records owned files, content hashes, applied config revisions, per-file policies, operation history, and restart state. It is the authority for what Gale may remove — a file absent from the publication but never recorded as deployed stays put.

The existing protections carry through both modes: managed vs host-managed BepInEx installations, restricted-root layouts, validated state-path adoption, retried removals, and server-config preservation during mods-only updates.

## Maintainer notes

The backend lives in `src-tauri/src/profile/server`:

- `plan.rs` — the pure planner; the single semantic source for previews and deploys in both modes.
- `engine.rs` — session, snapshot, lease-gated execution, restart, and state persistence shared by Local and Worker.
- `lease.rs` — the remote deployment lease (claim/heartbeat/ownership-verified release/stale takeover).
- `state.rs` — `.gale-server-state.json`: ownership records, config policies, operation history, restart flag.
- `spec.rs` — managed scope, loader-specific ownership, excluded paths, removal authority.
- `remote.rs` — SFTP/FTP/FTPS operations; FTPS certificate pinning and verification.
- `stage.rs` — publication → staged payloads; configs-only operations never touch mod sources.
- `commands.rs` — Tauri commands, executor dispatch (Local vs Worker), credentials, progress events.
- `runtime.rs` — the local server process and its profile lock, including the stopping state.
- `worker_client.rs` — desktop client for the worker API (loopback-only plaintext rule).
- `src-tauri/src/worker/` — the worker: HTTP API (`server.rs`), durable journal (`journal.rs`), config (`config.rs`), sync client (`sync_client.rs`), shared wire types (`api.rs`).

Frontend bindings are in `src/lib/api/profile/server.ts`, the sync dialog is `src/lib/components/dialogs/ServerSyncDialog.svelte`, and user-facing text lives in `messages/en.json` via Paraglide.

When adding another supported game, define its dedicated-server platforms and default port in `src-tauri/games.json`.
