use std::sync::{
    Arc,
    atomic::{AtomicU32, AtomicUsize, Ordering},
};

use chrono::Utc;
use eyre::Result;
use futures_util::future::BoxFuture;

use super::tests::{worker_config, worker_secrets};
use crate::{
    profile::server::{
        host::{HostControl, HostStatus},
        remote::fake_ftp::{FakeFtp, Options},
        settings::{RemoteProtocol, TransportSettings},
        state::{
            ExecutorKind, OperationKind, OperationRecord, OperationStatus, OperationSummary,
            RestartOutcome, ServerDeploymentState, VERSION,
        },
    },
    worker::{journal::Journal, secrets::Secrets},
};

const STATE_PATH: &str = "/BepInEx/config/.gale-server-state.json";
const LEASE_PATH: &str = "/BepInEx/config/.gale-deploy.lock";

#[derive(Clone)]
struct TestHost {
    ftp: Arc<FakeFtp>,
    players: Arc<AtomicU32>,
    restarts: Arc<AtomicUsize>,
    boot_probes: Arc<AtomicUsize>,
}

impl HostControl for TestHost {
    fn can_restart(&self) -> bool {
        true
    }

    fn restart<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            assert!(self.ftp.has_dir(LEASE_PATH));
            self.restarts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn status<'a>(&'a self) -> BoxFuture<'a, Result<HostStatus>> {
        Box::pin(async move {
            assert!(self.ftp.has_dir(LEASE_PATH));
            let booting = self.restarts.load(Ordering::SeqCst) > 0
                && self.boot_probes.fetch_add(1, Ordering::SeqCst) == 0;
            Ok(HostStatus {
                running: Some(true),
                booting: Some(booting),
                players: Some(self.players.load(Ordering::SeqCst)),
            })
        })
    }

    fn name(&self) -> &'static str {
        "Worker restart fixture"
    }
}

#[tokio::test]
async fn deferred_restart_survives_worker_restart_disabled_automation_and_failed_publication_poll()
{
    let dir = tempfile::tempdir().unwrap();
    let ftp = Arc::new(FakeFtp::valheim_host(Options::default()));
    ftp.seed_file("/BepInEx/config/mod.cfg", b"server configuration");
    ftp.seed_file("/BepInEx/plugins/mod.dll", b"deployed mod");
    let record = OperationRecord {
        id: "deferred-manual-deployment".into(),
        executor: ExecutorKind::Worker,
        kind: OperationKind::Manual,
        worker_id: Some("w".into()),
        publication_revision: Some(Utc::now()),
        mods_revision: None,
        status: OperationStatus::Succeeded,
        summary: OperationSummary {
            uploaded_files: 14,
            removed_files: 2,
            ..Default::default()
        },
        restart: RestartOutcome::AwaitingEmpty,
        error: None,
        started_at: Utc::now(),
        finished_at: Utc::now(),
    };
    let remote = ServerDeploymentState {
        version: VERSION,
        operation_seq: 1,
        restart_required: true,
        last_operation: Some(record.clone()),
        ..Default::default()
    };
    ftp.seed_file(STATE_PATH, &serde_json::to_vec(&remote).unwrap());
    let journal = Journal::load(dir.path()).unwrap();
    journal.record_operation(record).await.unwrap();
    drop(journal);

    let host = TestHost {
        ftp: ftp.clone(),
        players: Arc::new(AtomicU32::new(2)),
        restarts: Arc::new(AtomicUsize::new(0)),
        boot_probes: Arc::new(AtomicUsize::new(0)),
    };
    let mut config = worker_config(dir.path(), "127.0.0.1:0".into());
    config.sync_url = Some("http://127.0.0.1:9".into());
    config.remote.transport = TransportSettings {
        protocol: RemoteProtocol::Ftp,
        host: "127.0.0.1".into(),
        port: ftp.addr.port(),
        username: "u".into(),
        server_directory: "/".into(),
        ..Default::default()
    };
    let make_context = || {
        let mut ctx = super::WorkerContext::new(
            config.clone(),
            Secrets {
                remote_password: Some("pw".into()),
                ..worker_secrets()
            },
            Journal::load(dir.path()).unwrap(),
        )
        .unwrap();
        ctx.host = Box::new(host.clone());
        Arc::new(ctx)
    };
    let ctx = make_context();
    assert!(!ctx.journal.state.lock().await.auto_deploy_mods);
    // The default restart policy is Manual, but this deployment explicitly
    // requested WhenEmpty and its operation remains authoritative.
    super::poll_once(&ctx).await;
    assert_eq!(host.restarts.load(Ordering::SeqCst), 0);
    assert!(ctx.journal.state.lock().await.poll_error.is_some());
    assert!(!ftp.has_dir(LEASE_PATH));
    drop(ctx);

    host.players.store(0, Ordering::SeqCst);
    let ctx = make_context();
    let guard = ctx.operation_lock.lock().await;
    super::poll_once(&ctx).await;
    assert_eq!(
        host.restarts.load(Ordering::SeqCst),
        0,
        "manual work owns the operation lock"
    );
    drop(guard);
    super::poll_once(&ctx).await;

    let result: ServerDeploymentState =
        serde_json::from_slice(&ftp.file(STATE_PATH).unwrap()).unwrap();
    assert!(!result.restart_required);
    assert_eq!(
        result.last_operation.unwrap().restart,
        RestartOutcome::Restarted
    );
    assert_eq!(host.restarts.load(Ordering::SeqCst), 1);
    let journal = Journal::load(dir.path()).unwrap();
    let record = journal.state.lock().await.last_operation.clone().unwrap();
    assert_eq!(record.restart, RestartOutcome::Restarted);
    assert_eq!(record.summary.uploaded_files, 14);
    assert!(!ftp.has_dir(LEASE_PATH));
    assert_eq!(
        ftp.file("/BepInEx/config/mod.cfg").unwrap(),
        b"server configuration"
    );
    assert_eq!(ftp.count("RETR /BepInEx/config/mod.cfg"), 0);
    assert_eq!(
        ftp.file("/BepInEx/plugins/mod.dll").unwrap(),
        b"deployed mod"
    );
    assert_eq!(ftp.count("STOR /BepInEx/plugins/mod.dll"), 0);
    super::poll_once(&ctx).await;
    assert_eq!(host.restarts.load(Ordering::SeqCst), 1);
}
