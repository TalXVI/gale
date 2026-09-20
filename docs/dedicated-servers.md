# Dedicated servers

Gale can launch a dedicated server installed on the same computer, or deploy a published profile to a remote server. Dedicated server support only shows up for games with a `dedicatedServer` entry in `src-tauri/games.json`.

## Using it

Open the dedicated server dialog from the arrow beside the profile launch button.

### Local server

Choose **Local server**, enter the server name, world, port, and optional password, then select **Launch server**. Gale finds the game's dedicated server through its configured platform, prepares the mod loader, and starts the server with the active profile.

The server must already be installed. Gale manages only the process it starts, so closing Gale never kills an unrelated server process.

### Remote server

Choose **Remote server** and enter the connection details supplied by the server host.

- **SFTP (SSH)** supports password and private-key authentication. Confirm the host-key fingerprint the first time Gale connects.
- **FTP / FTPS (automatic)** tries encrypted explicit FTPS first and falls back to plain FTP only when the host does not support TLS. If you pin a certificate fingerprint, the connection requires that exact certificate, so a different CA-valid certificate does not satisfy the pin. Self-signed certificates require an explicit trust decision before Gale sends credentials.
- **Server directory** is the directory exposed by the host. Use `/` when the FTP account already opens at the server root.

Use **Test connection** before syncing.

## Selective server synchronization

The server is synchronized from the profile's **canonical publication**, the same revision friends' clients install. The owner publishes once, and clients and the dedicated server install the same artifact.

Open **Sync dedicated server** from the profile's server dialog.

### Scope

- **Mods only** deploys the published mod payload.
- **Selected configs only** deploys only the config files you choose.
- **Both** is the default.

### What the planner does

The preview combines the live remote listing with Gale's recorded deployment state:

- Gale updates the payload files it deployed to the published revision, and removes them when the publication drops them. If an owned file's contents changed on the server, Gale re-uploads it.
- Gale never deletes files it did not deploy. A manually installed `ServerOnly.dll` survives every sync and shows up as _unmanaged_ in the preview.
- Gale leaves world data, saves, logs, host-managed files, and anything outside the managed directories alone.
- Package-default configs only fill in files that are absent; they never overwrite a config the server already has.
- Disabling a mod produces `.old` files locally; Gale does not upload them.

### Config decisions and policies

For each revision, you can apply, decline, or leave each published config pending. If a config Gale deployed was deleted on the server, recreating it needs an explicit **Restore** decision.

Each config also has a persistent policy stored in the remote deployment state: **Ask each update**, **Always apply updates**, or **Always keep my config**. The policy survives switching between Local and Worker execution. A policy set during revision A only applies to future revisions; it does not decide the current conflict.

### Preview and approval

**Preview** computes the exact plan under the deployment lock: uploads, removals, config actions, and whether a restart is needed. **Deploy** runs it. The plan hash binds the approval, so if the remote state, the publication, the selection, or the restart policy changes between preview and deploy, Gale rejects the deploy and asks for a fresh preview. The dialog marks the approval stale automatically.

### Restart policy

Deployments that change mod payloads or apply config writes require a server restart to take effect. Choose per deployment:

- **Never (manual)** deploys only; restart it yourself.
- **Immediately** restarts as soon as the deployment lands.
- **When empty** restarts once no players are connected (requires a hosting provider that reports presence).

No-op, declined, or unselected changes do not trigger a restart.

## Execution modes

### Local

Gale on your PC connects directly to the server and deploys. Nothing else needs to run. Local mode is the default and works for manual deployments.

### Worker

`gale-worker` is a standalone binary that runs on the server host itself (or any always-on machine with access to it). It polls the sync service, deploys approved changes on its own, and can restart the server through the hosting provider's API. Your PC does not need to be online for automatic synchronization.

When to use it: automatic sync, or a host that is only reachable from a fixed location. Some game-panel providers do not allow running arbitrary persistent processes. In that case the worker must live on a separate always-on machine, like a VPS, a home server, or a NAS, that can reach the game server over FTP/SFTP and the provider's control API. File-management and restart APIs alone do not let a host run the worker.

#### Installing the worker

1. Build the binary: `cargo build --release --features worker --bin gale-worker` in `src-tauri`, or copy a prebuilt binary to the host.
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

   `profileId` and `game` pin the worker to one profile, so requests cannot redirect it elsewhere.

3. Provide secrets through the environment, never the config file:
   - `GALE_WORKER_TOKEN`: bearer token the API requires.
   - `GALE_WORKER_REMOTE_PASSWORD`: FTP/SFTP password or key passphrase.
   - `GALE_WORKER_REFRESH_TOKEN`: initial sync refresh token. The worker persists rotated tokens in its state journal afterwards.
   - `GALE_WORKER_DATHOST_PASSWORD`: DatHost API password, when applicable.

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

   The state journal (`gale-worker-state.json` in `stateDir`) records pending work, retry backoff, and rotated credentials. On POSIX the worker creates it owner-only, so keep `stateDir` out of shared directories. A restart resumes pending deployments instead of dropping them.

#### Connecting Gale to the worker

In the remote settings, choose **Worker** sync mode and enter the worker's address and token.

- Gale accepts `http://` **only** for a worker on the same machine (loopback). The bearer token travels in the clear over plaintext HTTP, so remote workers need `https://`. Terminate TLS with a reverse proxy (Caddy, nginx, Traefik) or reach the worker through a secure tunnel (WireGuard, Tailscale, SSH port-forward). Never expose the plaintext API to a LAN and assume it is safe.
- **Test connection** verifies reachability, the token, and that the worker's bound profile matches this profile.

Manual **Deploy** through the worker works regardless of the `autoSync` toggle. You can change the automation toggles (`autoSync`, `autoMods`) and the restart policy from the dialog, and they persist across worker restarts.

## Deployment coordination

Concurrent deployers coordinate through a lease directory claimed atomically on the remote (`/.gale-deploy.lock`). The holder keeps its lease alive with heartbeats while it works. Release and heartbeat both verify the record still belongs to that operation, so a stale executor cannot delete a newer executor's lease.

Guarantees:

- Two executors cannot hold the lease at once. The second one gets a busy error.
- A crashed executor's lease expires. Once its heartbeat is stale past the TTL, a new executor may take it over. The UI shows the stale lease and offers an explicit takeover. `force` never displaces a live lease.
- An executor that loses the lease stops making changes. State writes carry the operation sequence, so a stale writer cannot overwrite newer state.
- Config-policy writes run under the same lease as deployments.

One limitation matters here: FTP/SFTP provide no true fencing, so a write that was already in flight from a lost-lease executor can still land. The conservative answer is built in. Gale never auto-breaks a lease that might still be live. If a lease is stuck because an executor hung, confirm it is actually stopped (check the machine or the worker status), then use the takeover option in the sync dialog. Treat takeover as the documented recovery path, not routine automation.

## What is preserved

The deployment state file (`.gale-server-state.json` in the server directory, or under `config/` in restricted-root layouts) records owned files, content hashes, applied config revisions, per-file policies, operation history, and restart state. It is the authority for what Gale may remove. A file absent from the publication but never recorded as deployed stays put.

The existing protections apply in both modes: managed vs host-managed BepInEx installations, restricted-root layouts, validated state-path adoption, retried removals, and server-config preservation during mods-only updates.

## Maintainer notes

The backend lives in `src-tauri/src/profile/server`:

- `plan.rs`: the pure planner. It is the only place sync semantics are decided, shared by previews and deploys in both modes.
- `engine.rs`: session, snapshot, lease-gated execution, restart, and state persistence shared by Local and Worker.
- `lease.rs`: the remote deployment lease (claim/heartbeat/ownership-verified release/stale takeover).
- `state.rs`: `.gale-server-state.json`, holding ownership records, config policies, operation history, and the restart flag.
- `spec.rs`: managed scope, loader-specific ownership, excluded paths, removal authority.
- `remote.rs`: SFTP/FTP/FTPS operations; FTPS certificate pinning and verification.
- `stage.rs`: publication → staged payloads; configs-only operations never touch mod sources.
- `commands.rs`: Tauri commands, executor dispatch (Local vs Worker), credentials, progress events.
- `runtime.rs`: the local server process and its profile lock, including the stopping state.
- `worker_client.rs`: desktop client for the worker API (loopback-only plaintext rule).
- `src-tauri/src/worker/`: the worker, with its HTTP API (`server.rs`), durable journal (`journal.rs`), config (`config.rs`), sync client (`sync_client.rs`), and shared wire types (`api.rs`).

Frontend bindings are in `src/lib/api/profile/server.ts`, the sync dialog is `src/lib/components/dialogs/ServerSyncDialog.svelte`, and user-facing text lives in `messages/en.json` via Paraglide.

When adding another supported game, define its dedicated-server platforms and default port in `src-tauri/games.json`.
