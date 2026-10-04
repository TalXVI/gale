use std::time::Duration;

use chrono::{TimeDelta, Utc};

use super::tests::{free_port, pack_manifest, publication_zip, worker_config, worker_secrets};
use crate::{
    profile::{
        server::{
            executor::SyncExecutor,
            plan::DeploySelection,
            remote::fake_ftp::{FakeFtp, Options},
            settings::{RemoteProtocol, RemoteServerSettings, RestartPolicy, TransportSettings},
            state::{OperationKind, ServerDeploymentState, VERSION},
            worker_client::WorkerClient,
        },
        sync::{SyncProfileMetadata, auth::User, publication_from_archive},
    },
    thunderstore::Backend,
    worker::{
        api::PreviewRequest,
        journal::Journal,
        secrets::Secrets,
        sync_client::tests::{MockApi, serve},
    },
};

// Exercise the real client, HTTP route, journal, automation loop, and FTP engine.
// Only the external Sync/FTP services are fakes; package staging is cached offline.
#[tokio::test]
async fn live_refresh_observes_new_publication_and_wakes_config_free_automation() {
    for auto_deploy in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let old = Utc::now() - TimeDelta::minutes(1);
        let new = old + TimeDelta::seconds(1);
        let mut manifest = pack_manifest();
        let mut metadata = SyncProfileMetadata {
            id: "sync-profile-1".to_owned(),
            created_at: old,
            updated_at: old,
            owner: User {
                discord_id: "1".to_owned(),
                name: "owner".to_owned(),
                display_name: "Owner".to_owned(),
                avatar: None,
            },
            manifest: manifest.clone(),
        };
        let archive = publication_zip(&manifest);
        let previous = publication_from_archive(&metadata, &archive).unwrap();
        let sync = serve(MockApi {
            meta: Some(metadata.clone()),
            archive,
            ..Default::default()
        })
        .await;
        let ftp = FakeFtp::valheim_host(Options::default());
        ftp.seed_file("/BepInEx/config/mod.cfg", b"server-owned");
        ftp.seed_file("/BepInEx/plugins/unmanaged.dll", b"unmanaged");
        let remote = ServerDeploymentState {
            version: VERSION,
            mods_revision: Some(previous.mods_revision.clone()),
            ..Default::default()
        };
        ftp.seed_file(
            "/BepInEx/config/.gale-server-state.json",
            &serde_json::to_vec(&remote).unwrap(),
        );
        let journal = Journal::load(dir.path()).unwrap();
        {
            let mut state = journal.state.lock().await;
            state.automation_seeded = true;
            state.auto_deploy_mods = auto_deploy;
            state.last_deployed_revision = Some(old);
            state.deployed_mods_revision = Some(previous.mods_revision);
            journal.save(&state).unwrap();
        }
        let mut config = worker_config(dir.path(), format!("127.0.0.1:{}", free_port()));
        config.profile_id = metadata.id.clone();
        config.poll_interval_secs = 300;
        config.sync_url = Some(sync.url.clone());
        config.remote = RemoteServerSettings {
            transport: TransportSettings {
                protocol: RemoteProtocol::Ftp,
                host: "127.0.0.1".to_owned(),
                port: ftp.addr.port(),
                username: "u".to_owned(),
                server_directory: "/".to_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        let mut settings = config.remote.clone();
        settings.executor.worker_address = format!("http://{}", config.listen);
        let client =
            WorkerClient::new(&settings.executor.worker_address, "token".to_owned()).unwrap();
        let stop = tokio_util::sync::CancellationToken::new();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(super::run(
            config,
            Secrets {
                remote_password: Some("pw".to_owned()),
                refresh_token: Some("refresh-seed".to_owned()),
                ..worker_secrets()
            },
            stop.clone(),
            Some(ready_tx),
        ));
        ready_rx.await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while sync.metadata_requests() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            client.status(false).await.unwrap().observed_revision,
            Some(old)
        );

        manifest.mods[0].version = semver::Version::new(2, 0, 0).into();
        metadata.updated_at = new;
        metadata.manifest = manifest.clone();
        sync.publish(metadata.clone(), publication_zip(&manifest));
        let staged = crate::profile::install::cache::path(
            &dir.path().join("cache"),
            &crate::thunderstore::VersionIdent::from(("Author", "Mod", "2.0.0")),
            Backend::Thunderstore,
        );
        std::fs::create_dir_all(staged.join("BepInEx/config")).unwrap();
        std::fs::create_dir_all(staged.join("BepInEx/plugins/Author-Mod")).unwrap();
        std::fs::write(staged.join("BepInEx/plugins/Author-Mod/mod.dll"), b"v2").unwrap();
        std::fs::write(
            staged.join("BepInEx/config/packaged.cfg"),
            b"package-default",
        )
        .unwrap();
        let metadata_reads = sync.metadata_requests();
        let archive_reads = sync.archive_requests();
        let remote_commands = ftp.commands.lock().unwrap().len();
        for _ in 0..3 {
            let status = client.status(false).await.unwrap();
            assert_eq!(status.observed_revision, Some(old));
            assert!(status.pending_revision.is_none() && status.server.is_none());
        }
        assert_eq!(
            sync.metadata_requests(),
            metadata_reads,
            "background reads must not poll Sync"
        );
        assert_eq!(
            sync.archive_requests(),
            archive_reads,
            "background reads must not download archives"
        );
        assert_eq!(
            ftp.commands.lock().unwrap().len(),
            remote_commands,
            "background reads must not connect remotely"
        );

        let gate = sync.hold_deployment_archive();
        let refreshed = tokio::time::timeout(Duration::from_secs(5), client.status(true))
            .await
            .expect("refresh must not wait for automatic deployment")
            .unwrap();
        assert_eq!(
            refreshed.observed_revision,
            Some(new),
            "refresh must observe N+1 without restarting"
        );
        assert_eq!(refreshed.pending_revision, Some(new));
        assert_eq!(refreshed.last_deployed_revision, Some(old));
        if !auto_deploy {
            client.configure(true, RestartPolicy::Manual).await.unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while sync.archive_requests() < archive_reads + 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("new work must wake automation before the 300-second tick");
        let busy = tokio::time::timeout(Duration::from_secs(5), client.status(true))
            .await
            .unwrap()
            .unwrap();
        assert!(busy.busy);
        assert_eq!(busy.pending_revision, Some(new));
        let error = client
            .preview(PreviewRequest {
                selection: DeploySelection::default(),
                restart_policy: None,
                run_id: "manual-race".to_owned(),
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already running"));
        gate.add_permits(10);
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let status = client.status(false).await.unwrap();
                if !status.busy
                    && status.pending_revision.is_none()
                    && status.last_deployed_revision == Some(new)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("automatic deployment must finish without a Worker restart");
        let deployed: ServerDeploymentState =
            serde_json::from_slice(&ftp.file("/BepInEx/config/.gale-server-state.json").unwrap())
                .unwrap();
        assert!(
            deployed.config.is_empty(),
            "automatic sync must create no config state debt"
        );
        assert!(deployed.restart_required);
        let operation = deployed.last_operation.unwrap();
        assert_eq!(operation.kind, OperationKind::Automatic);
        assert_eq!(operation.summary.config_writes, 0);
        assert_eq!(
            ftp.file("/BepInEx/config/mod.cfg").unwrap(),
            b"server-owned"
        );
        assert!(ftp.file("/BepInEx/config/packaged.cfg").is_none());
        assert_eq!(
            ftp.file("/BepInEx/plugins/unmanaged.dll").unwrap(),
            b"unmanaged"
        );
        {
            let commands = ftp.commands.lock().unwrap();
            assert!(
                !commands
                    .iter()
                    .any(|line| line.contains("mod.cfg") || line.contains("packaged.cfg")),
                "routine sync must not read, hash, seed, or write configs: {commands:?}"
            );
        }
        stop.cancel();
        task.await.unwrap().unwrap();
    }
}
