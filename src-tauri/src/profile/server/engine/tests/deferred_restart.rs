use std::{
    collections::VecDeque,
    sync::atomic::{AtomicUsize, Ordering},
};

use futures_util::future::BoxFuture;

use super::*;

struct TestHost {
    remote: Shared,
    statuses: Mutex<VecDeque<Result<HostStatus>>>,
    probes: AtomicUsize,
    restarts: AtomicUsize,
    fail_restart: bool,
    displace_lease: bool,
    fail_final_state_write: bool,
}

impl TestHost {
    fn new(remote: &Shared, statuses: Vec<Result<HostStatus>>) -> Self {
        Self {
            remote: remote.clone(),
            statuses: Mutex::new(statuses.into()),
            probes: AtomicUsize::new(0),
            restarts: AtomicUsize::new(0),
            fail_restart: false,
            displace_lease: false,
            fail_final_state_write: false,
        }
    }
}

impl HostControl for TestHost {
    fn can_restart(&self) -> bool {
        true
    }

    fn restart<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            assert!(remote_has_dir(&self.remote, LEASE_DIR_REMOTE));
            assert_eq!(
                remote_state(&self.remote).last_operation.unwrap().restart,
                RestartOutcome::StartupUnverified,
            );
            self.restarts.fetch_add(1, Ordering::SeqCst);
            if self.fail_restart {
                bail!("restart rejected");
            }
            if self.displace_lease {
                let mut remote = self.remote.lock().unwrap();
                let mut record: LeaseRecord =
                    serde_json::from_slice(remote.contents(LEASE_FILE_REMOTE).unwrap()).unwrap();
                record.owner = "foreign executor".into();
                record.operation_id = "foreign operation".into();
                remote.put_file(LEASE_FILE_REMOTE, &serde_json::to_vec(&record).unwrap());
            }
            if self.fail_final_state_write {
                self.remote
                    .lock()
                    .unwrap()
                    .fail_always
                    .insert(format!("{STATE_REMOTE}.tmp"));
            }
            Ok(())
        })
    }

    fn status<'a>(&'a self) -> BoxFuture<'a, Result<HostStatus>> {
        Box::pin(async move {
            self.probes.fetch_add(1, Ordering::SeqCst);
            assert!(remote_has_dir(&self.remote, LEASE_DIR_REMOTE));
            self.statuses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(status(Some(0), false)))
        })
    }

    fn name(&self) -> &'static str {
        "deferred restart fixture"
    }
}

fn status(players: Option<u32>, booting: bool) -> HostStatus {
    HostStatus {
        running: Some(true),
        booting: Some(booting),
        players,
    }
}

fn pending_remote() -> Shared {
    let memory = remote();
    let mut record = meta().record(OperationStatus::Succeeded, RestartOutcome::AwaitingEmpty);
    record.id = "deferred".into();
    record.summary.uploaded_files = 1;
    let state = ServerDeploymentState {
        version: state::VERSION,
        operation_seq: 1,
        restart_required: true,
        last_operation: Some(record),
        ..Default::default()
    };
    {
        let mut remote = memory.lock().unwrap();
        remote.put_file(STATE_REMOTE, &state::serialize(&state).unwrap());
        remote.put_file(MOD_DLL_REMOTE, b"deployed mod");
        remote.put_file("/srv/BepInEx/config/mod.cfg", b"runtime config");
    }
    memory
}

async fn resume(memory: &Shared, host: &TestHost) -> Result<ServerDeploymentState> {
    let heartbeat = memory.clone();
    super::super::resume_deferred_restart(
        open(memory.clone())?,
        move || Ok(Box::new(heartbeat.clone())),
        "deferred".into(),
        host,
        meta(),
        &mut ProgressReporter::silent(SyncOperation::Deploy, &selection(false, false)),
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn restarts_when_the_last_player_leaves_without_redeploying_files() {
    let memory = pending_remote();
    let host = TestHost::new(
        &memory,
        vec![
            Ok(status(Some(2), false)),
            Ok(status(Some(0), false)),
            Ok(status(Some(0), true)),
            Ok(status(Some(0), false)),
        ],
    );
    let waiting = resume(&memory, &host).await.unwrap();
    assert!(waiting.restart_required);
    assert_eq!(waiting.operation_seq, 1);
    assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));

    let restarted = resume(&memory, &host).await.unwrap();
    assert!(!restarted.restart_required);
    assert_eq!(restarted.operation_seq, 3);
    let record = restarted.last_operation.unwrap();
    assert_eq!(record.restart, RestartOutcome::Restarted);
    assert_eq!(record.summary.uploaded_files, 1);
    assert_eq!(host.restarts.load(Ordering::SeqCst), 1);
    assert!(!remote_state(&memory).restart_required);
    assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
    {
        let files = &memory.lock().unwrap().files;
        assert_eq!(files.get(MOD_DLL_REMOTE).unwrap(), b"deployed mod");
        assert_eq!(
            files.get("/srv/BepInEx/config/mod.cfg").unwrap(),
            b"runtime config"
        );
    }

    resume(&memory, &host).await.unwrap();
    assert_eq!(host.restarts.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn unknown_presence_or_a_failed_status_probe_keeps_the_restart_pending() {
    for presence in [
        Ok(status(None, false)),
        Err(eyre::eyre!("probe unavailable")),
    ] {
        let memory = pending_remote();
        let before = memory.lock().unwrap().files.clone();
        let host = TestHost::new(&memory, vec![presence]);
        let result = resume(&memory, &host).await.unwrap();
        assert!(result.restart_required);
        assert_eq!(
            result.last_operation.unwrap().restart,
            RestartOutcome::AwaitingEmpty
        );
        assert_eq!(host.restarts.load(Ordering::SeqCst), 0);
        assert_eq!(memory.lock().unwrap().files, before);
    }
}

#[tokio::test(start_paused = true)]
async fn a_superseded_deployment_is_not_restarted() {
    let memory = pending_remote();
    let host = TestHost::new(&memory, vec![]);
    let stale_session = open(memory.clone()).unwrap();
    let mut newer = remote_state(&memory);
    newer.operation_seq = 2;
    newer.last_operation.as_mut().unwrap().id = "newer deployment".into();
    memory
        .lock()
        .unwrap()
        .put_file(STATE_REMOTE, &state::serialize(&newer).unwrap());
    let before = memory.lock().unwrap().files.clone();

    let heartbeat = memory.clone();
    let result = super::super::resume_deferred_restart(
        stale_session,
        move || Ok(Box::new(heartbeat.clone())),
        "deferred".into(),
        &host,
        meta(),
        &mut ProgressReporter::silent(SyncOperation::Deploy, &selection(false, false)),
    )
    .await
    .unwrap();
    assert_eq!(result.last_operation.unwrap().id, "newer deployment");
    assert_eq!(host.probes.load(Ordering::SeqCst), 0);
    assert_eq!(memory.lock().unwrap().files, before);
}

#[tokio::test(start_paused = true)]
async fn a_live_foreign_lease_blocks_the_deferred_restart() {
    let memory = pending_remote();
    let host = TestHost::new(&memory, vec![]);
    let mut holder = open(memory.clone()).unwrap();
    let lease = holder
        .acquire_lease(&OperationMeta::local("other", OperationKind::Manual), false)
        .unwrap();

    let error = resume(&memory, &host).await.unwrap_err();
    assert!(error.downcast_ref::<lease::LeaseBusy>().is_some());
    assert_eq!(host.probes.load(Ordering::SeqCst), 0);
    lease.release(holder.ops.as_mut());
}

#[tokio::test(start_paused = true)]
async fn failed_or_unverified_restarts_remain_required_and_are_not_requested_again() {
    for (fail_restart, expected) in [
        (true, RestartOutcome::Failed),
        (false, RestartOutcome::StartupUnverified),
    ] {
        let memory = pending_remote();
        let mut host = TestHost::new(&memory, vec![Ok(status(Some(0), false))]);
        host.fail_restart = fail_restart;
        let result = resume(&memory, &host).await.unwrap();
        assert!(result.restart_required);
        assert_eq!(result.last_operation.unwrap().restart, expected);
        assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
        resume(&memory, &host).await.unwrap();
        assert_eq!(host.restarts.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn losing_the_lease_prevents_clearing_restart_state() {
    let memory = pending_remote();
    let mut host = TestHost::new(
        &memory,
        vec![
            Ok(status(Some(0), false)),
            Ok(status(Some(0), true)),
            Ok(status(Some(0), false)),
        ],
    );
    host.displace_lease = true;

    let error = resume(&memory, &host).await.unwrap_err();
    assert!(error.to_string().contains("taken over"), "{error:#}");
    assert!(remote_state(&memory).restart_required);
    assert!(remote_has_dir(&memory, LEASE_DIR_REMOTE));
}

#[tokio::test(start_paused = true)]
async fn a_failed_final_state_write_cannot_request_the_same_restart_again() {
    let memory = pending_remote();
    let mut host = TestHost::new(
        &memory,
        vec![
            Ok(status(Some(0), false)),
            Ok(status(Some(0), true)),
            Ok(status(Some(0), false)),
        ],
    );
    host.fail_final_state_write = true;

    assert!(resume(&memory, &host).await.is_err());
    let persisted = remote_state(&memory);
    assert!(persisted.restart_required);
    assert_eq!(
        persisted.last_operation.unwrap().restart,
        RestartOutcome::StartupUnverified
    );
    assert!(!remote_has_dir(&memory, LEASE_DIR_REMOTE));
    resume(&memory, &host).await.unwrap();
    assert_eq!(host.restarts.load(Ordering::SeqCst), 1);
}
