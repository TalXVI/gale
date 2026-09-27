# Dedicated servers

Gale can launch a dedicated server on this computer or deploy a published profile to a remote server. Open the **Dedicated server** page from the navigation bar. It appears only for games with a `dedicatedServer` entry in `src-tauri/games.json`.

## Local server

Choose **This computer**, enter the server name, world, port, and optional password, then select **Launch server**. Gale finds the game's dedicated server through its configured platform, prepares the mod loader, and starts the server with the active profile.

Select **Save settings** to save the local settings and, when **Remember password** is selected, the game password. With that option off, launch from the page to use the password for this session only. While the server runs, Gale shows its status and locks the profile. Use the server console for a normal shutdown; **Force stop server** is available if it does not respond and warns that unsaved progress may be lost.

The server must already be installed. Gale manages only the process it starts, so closing Gale never kills an unrelated server process.

## Remote server

Choose **Remote server** and enter the connection details supplied by the server host.

- **SFTP (SSH)** supports password and private-key authentication. Confirm the host-key fingerprint the first time Gale connects.
- **FTPS (TLS required)** encrypts FTP with TLS and fails instead of falling back to plaintext. If you pin a certificate fingerprint, the connection requires that exact certificate, so a different CA-valid certificate does not satisfy the pin. Self-signed certificates require an explicit trust decision before Gale sends credentials.
- **FTP (automatic TLS, allows plaintext fallback)** tries encrypted FTPS first and falls back to plain FTP when the host does not support TLS. The same certificate-pinning rules apply while TLS is active.
- **Server directory** is the directory exposed by the host and defaults to `/`, which fits accounts that already open at the server root.

Use **Test connection** before syncing. Tests do not save settings or credentials. Unsaved edits show a bar at the bottom of the page: select **Save settings** to keep them or **Discard** to restore the saved values. Deploy stays disabled until the connection settings are saved and clean.

## Selective server synchronization

The server is synchronized from the profile's **canonical publication**, the same revision friends' clients install. The owner publishes once, and clients and the dedicated server install the same artifact.

The status and deploy panel sits at the top of the **Remote server** tab.

### What gets synchronized

**Preview** and **Deploy** handle published mods. To write config files, use a separate config push. The server owns its configs after setup because mods can change them at startup. Config differences do not create pending work.

To seed a fresh server or intentionally replace a config file, open **Server config files** on the Server page and select **Preview config changes**, then **Push configs**. The push uses the same planner, lease, and approval rules as a mod deployment. Once it completes, Gale does not track the pushed files as outstanding work.

### What the planner does

The preview combines the live remote listing with Gale's recorded deployment state:

- Gale updates the payload files it deployed to the published revision, and removes them when the publication drops them. If an owned file's contents changed on the server, Gale re-uploads it.
- Gale never deletes files it did not deploy. A manually installed `ServerOnly.dll` survives every sync and shows up as _unmanaged_ in the preview.
- Gale leaves world data, saves, logs, host-managed files, and anything outside the managed directories alone.
- Config files bundled inside mod packages are never deployed. The running server generates or migrates its own configs, and only an explicit config push writes them.
- Disabling a mod produces `.old` files locally; Gale does not upload them.

### Config decisions and policies

A config push lists every published config file with its planned action. You can apply, decline, or leave each conflicted config pending. If a config Gale deployed was deleted on the server, recreating it needs an explicit **Restore** decision.

Each config also has a persistent policy stored in the remote deployment state: **Ask each update**, **Always apply updates**, or **Always keep my config**. The policy survives switching between Local and Worker execution. A policy set during revision A only applies to future revisions; it does not decide the current conflict.

### Preview and approval

**Preview** computes the exact mod plan under the deployment lock: uploads, removals, and whether a restart is needed. **Deploy** runs it. **Preview config changes** computes the same kind of plan for a config push, including per-file actions. The plan hash binds the approval, so if the remote state, the publication, the selection, or the restart policy changes between preview and deploy, Gale rejects the deploy and asks for a fresh preview. The page marks the approval stale automatically.

### Restart policy

Deployments that change mod payloads or apply config writes require a server restart to take effect. Choose per deployment:

- **Never** deploys only; restart it yourself.
- **Immediately** restarts as soon as the deployment lands.
- **When empty** restarts once no players are connected (requires a hosting provider that reports presence).

No-op, declined, or unselected changes do not trigger a restart.

After restarting through the host panel, use **I confirmed the restart** in the remote status panel. Gale records that confirmation in the remote deployment state. A Gale-issued restart clears the reminder only after the host reports a stop followed by a start; a running status by itself leaves the restart unverified.

## Execution modes

### Local

Gale on your PC connects directly to the server and deploys. Nothing else needs to run. Local mode is the default.

### Worker

`gale-worker` is a standalone binary that polls the sync service, deploys approved changes on its own, and can restart the server through the hosting provider's API. It can run on this PC as a managed Windows service, or on a separate always-on machine (a VPS, home server, or NAS) that can reach the game server over FTP/SFTP and the provider's control API.

When to use it: automatic mod deployment, or a host that is only reachable from a fixed location. Some game-panel providers do not allow running arbitrary persistent processes; file-management and restart APIs alone do not let a host run the worker.

#### Hosting the worker on this PC (Windows)

Choose **Worker on this PC** as the sync mode and select **Set up worker**. Gale then:

1. Saves the remote connection settings in Local mode. A fresh profile has no worker address, so it cannot enter Worker mode yet.
2. Opens a browser sign-in to give the worker its own Gale sync credentials. Refresh tokens rotate, so sharing the desktop's token would break one of the sessions.
3. Stages a worker config and credentials file in a permission-restricted temp directory, then shows **one UAC elevation prompt** that installs and starts the `GaleWorker` Windows service.
4. Points this profile's sync mode at the worker's loopback address and stores its bearer token.

If service installation succeeds but saving the profile settings fails, the page reports _setup incomplete_. Run **Set up worker** again to sign in, reinstall the worker, and finish linking the profile. Gale keeps pending work and automation settings when the profile and remote server have not changed.

The service runs as LocalSystem. It starts with Windows before sign-in, restarts after a crash, and resumes queued deployments from its journal. It reports `Running` only after its API starts serving. An initialization failure reports `Stopped` with a failure exit so Windows can restart it. Start, Stop, and Restart use the service's control permissions. Update and Uninstall request UAC elevation.

The notification-area icon belongs to a per-user helper separate from the Session 0 service. Gale copies the helper to a directory named for its content under `%LocalAppData%\Gale\worker-tray`, registers that copy in `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, and starts it after setup. The helper checks the service state. It shows the Gale icon and `Gale Worker` tooltip while the service is `Running`, then hides the icon when the service stops. A session-local mutex prevents duplicate icons.

After an app update, Gale refreshes the helper copy at startup. Uninstalling the worker removes the startup entry, stops the helper, and removes its user-local files.

State layout:

```text
C:\ProgramData\Gale\worker\
  gale-worker.json     worker configuration
  status.json          last run report (running/stopped/shutdown)
  worker.log           service log
  gale-worker.exe      the copy the service actually runs
  private\             ACL'd to SYSTEM + Administrators only
    secrets.env        API token, remote credential, sync refresh token
    gale-worker-state.json   durable journal (pending work, rotated tokens)
    gale-worker.lock   instance lock that blocks a second worker
    ssh.key            staged copy of the SSH key, when key auth is used
```

Notes and limitations:

- Windows only. On Linux/macOS the page does not offer this option; use **Worker on another machine**.
- SSH **agent** authentication cannot run unattended in a service; use a password or a private key. A private key under `%USERPROFILE%` is copied into `private\` at setup, since LocalSystem cannot read your profile directory.
- One managed worker per machine. The `GaleWorker` service name and state-directory lock reject duplicates. The worker stays bound to its setup profile. Other profiles cannot start, stop, update, or uninstall it. To move it, sign in to the owning profile and uninstall first.
- Gale updates ship a newer `gale-worker.exe` beside the app, but the service keeps running its installed copy so updates never fight a locked executable. The page shows **Update worker** when the bundled copy is newer; updating keeps credentials and pending work.
- The tray process runs only from its LocalAppData copy and embeds Gale's existing icon. It therefore holds neither the installed service binary nor Gale's bundled Worker/tray files open. Its **Update** command passes the current bundled `gale-worker.exe` to the same elevated `service reinstall` path as the Server page; the tray helper can never become the service update source.
- `status.json` records why the worker last stopped. If the service is stopped but the report says `running`, the process crashed. Windows service recovery restarts it. A `shutdown` report means the machine went down and the service returns on the next boot.

#### Installing the worker on a separate host

1. Build the binary: `cargo build --release --features worker --bin gale-worker` in `src-tauri`, or copy a prebuilt binary to the host.
2. Create `gale-worker.json` (see `src-tauri/src/worker/config.rs` for the full reference):

   <!-- prettier-ignore -->
   ```json
   {
     "workerId": "my-vps",
     "listen": "127.0.0.1:8472",
     "profileId": "the sync profile id",
     "game": "valheim",
     "remote": {
       "protocol": "sftp", "host": "...", "username": "...",
       "hostControl": { "provider": "datHost", "datHostServerId": "...", "datHostUsername": "..." },
       "worker": { "autoDeployMods": true },
       "restartPolicy": "whenEmpty"
     },
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

In the remote settings, choose **Worker on another machine** as the sync mode and enter the worker's address and token.

- Gale accepts `http://` **only** for a worker on the same machine (loopback). The bearer token travels in the clear over plaintext HTTP, so remote workers need `https://`. Terminate TLS with a reverse proxy (Caddy, nginx, Traefik) or reach the worker through a secure tunnel (WireGuard, Tailscale, SSH port-forward). Never expose the plaintext API to a LAN and assume it is safe.
- **Test connection** verifies reachability, the token, and that the worker's bound profile matches this profile.

Manual **Deploy** through the worker works regardless of `autoDeployMods`. You can change automatic mod deployment and the independent restart policy in the **Deployment** section of the Server page. Gale pushes the values to the running worker, reads them back, and they persist across worker restarts.

The worker tracks the publication's mod payload. It marks a publication pending when its mod revision differs from the deployed revision. A successful mod deployment clears that pending work. Config-only publications settle without a deployment.

The worker polls for publications whether automatic deployment is on or off. With `autoDeployMods` off, mod work stays pending until a manual deploy runs or you turn automation on. The worker never reads or writes config files automatically. To write them, use **Push configs** on the Server page.

#### Moving a managed worker to a VPS

The managed worker's job queue and rotated credentials live in `%ProgramData%\Gale\worker\private`. To move hosting to an external machine without losing pending work or re-doing the sign-in:

1. Stop the old worker first. On the Server page choose **Stop**, wait until the service reports `stopped`, and confirm the last `status.json` report is not `running`. Two workers can keep polling the same server even though the remote lease prevents simultaneous deployments.
2. **Copy the durable state** to the VPS: `private\gale-worker-state.json` (journal: pending work, retry backoff, rotated refresh token) and `private\secrets.env` (API token and credentials). Copy `gale-worker.json` too as a starting point.
3. On the VPS, write a new `gale-worker.json` with the same `profileId`, `game`, and remote settings. Point `stateDir` at the copied journal and set `listen` to an address Gale can reach. Load the copied secrets through `secretsFile` or the environment. Use the worker's rotated token from `secrets.env`, not the desktop's sign-in.
4. Start the external worker (systemd example above), then switch the profile's sync mode to **Worker on another machine** with the new address and the same bearer token.
5. **Uninstall the local service** from the Server page (**Uninstall**) once the external worker reports healthy. Uninstall deletes `%ProgramData%\Gale\worker`, so copy the state out first.
6. Verify that the new worker is bound to this profile and that `pendingRevision` clears after the first poll. Gale warns if the worker belongs to a different profile.

Without the copied state, the new worker starts with an empty journal. Set up fresh worker credentials on the VPS in that case.

## Deployment coordination

Concurrent deployers coordinate through a lease directory claimed atomically on the remote (`BepInEx/config/.gale-deploy.lock`, or `config/.gale-deploy.lock` in restricted-root layouts). The holder keeps its lease alive with heartbeats while it works. Release and heartbeat both verify the record still belongs to that operation, so a stale executor cannot delete a newer executor's lease.

Guarantees:

- Two executors cannot hold the lease at once. The second one gets a busy error.
- A crashed executor's lease expires. Once its heartbeat is stale past the TTL, a new executor may take it over. The UI shows the stale lease and offers an explicit takeover. `force` never displaces a live lease.
- An executor that loses the lease stops making changes. State writes carry the operation sequence, so a stale writer cannot overwrite newer state.
- Config-policy writes run under the same lease as deployments.

FTP and SFTP cannot stop a write already in flight when its executor loses the lease. Gale therefore never takes over a lease that might still be live. If an executor hangs, check its machine or worker status to confirm it has stopped. Then use the takeover option on the Server page.

DatHost's FTP refuses `SIZE` in ASCII mode with `550 SIZE not allowed in ASCII mode`. Gale sets binary mode with `TYPE I` when it connects. It then uses `MLST` and `RETR` to check whether a file exists and read its contents. A refused `SIZE` cannot make an existing file look absent.

Some hosts also filter reads of the lease record. Gale places a `holder-<operation>` directory inside the claim, which it can check with directory probes. The marker keeps the claim directory nonempty, so another executor cannot silently remove a claim it cannot inspect. If a crashed executor leaves a claim on such a host, confirm that no executor is running, then remove `.gale-deploy.lock` through the host's file manager.

A deployment only reports success once its state file reads back byte-identical. A host that can accept writes but cannot return them fails the deployment rather than silently losing the ownership records later operations depend on.

## What is preserved

The deployment state file (`BepInEx/config/.gale-server-state.json`, or `config/.gale-server-state.json` in restricted-root layouts) records owned files, content hashes, applied config revisions, per-file policies, the last operation, and restart state. It is the authority for what Gale may remove. A file absent from the publication but never recorded as deployed stays put.

The same protections apply in both modes: host-managed BepInEx installations are left alone, restricted-root layouts are detected, a failed removal keeps its ownership record so the next deployment retries it, and mods-only updates never touch server configs.

## Maintainer notes

The backend lives in `src-tauri/src/profile/server`:

- `plan.rs`: the pure planner. It is the only place sync semantics are decided, shared by previews and deploys in both modes.
- `engine.rs`: session, snapshot, lease-gated execution, restart, and state persistence shared by Local and Worker.
- `lease.rs`: the remote deployment lease (claim/heartbeat/ownership-verified release/stale takeover).
- `state.rs`: `.gale-server-state.json`, holding ownership records, config policies, the last operation, and the restart flag.
- `spec.rs`: managed scope, loader-specific ownership, excluded paths, removal authority.
- `remote.rs`: SFTP/FTP/FTPS operations; FTPS certificate pinning and verification.
- `stage.rs`: publication → staged payloads; configs-only operations never touch mod sources.
- `commands.rs`: Tauri commands, executor dispatch (Local vs Worker), credentials, progress events.
- `local_worker.rs`: desktop orchestration for the managed Windows worker, including provisioning, elevated install and uninstall, service control, and the ProgramData layout.
- `runtime.rs`: the local server process and its profile lock, including the stopping state.
- `worker_client.rs`: desktop client for the worker API (loopback-only plaintext rule).
- `src-tauri/src/worker/`: the HTTP API (`server.rs`), journal (`journal.rs`), config (`config.rs`), secrets loader (`secrets.rs`), sync client (`sync_client.rs`), shared API types (`api.rs`), managed-service paths (`local.rs`), and Windows service code (`service.rs`). Pending journal work always means a mod payload is owed.

Frontend bindings are in `src/lib/api/profile/server.ts`, the page is `src/lib/components/server/ServerPage.svelte`, and user-facing text lives in `messages/en.json` via Paraglide.

When adding another supported game, define its dedicated-server platforms and default port in `src-tauri/games.json`.
