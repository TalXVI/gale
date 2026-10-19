//! Local, release-mode polling benchmarks. All network endpoints and package
//! caches are fixtures. Run with scripts/bench-worker-poll.ps1.

use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{Request, State},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::Utc;
use eyre::Result;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::tests::{pack_manifest, worker_config, worker_secrets};
use crate::{
    profile::{
        server::{
            host::{HostControl, HostStatus},
            remote::fake_ftp::{FakeFtp, Options},
            settings::{RemoteProtocol, TransportSettings},
            state::{
                ExecutorKind, OperationKind, OperationRecord, OperationStatus, OperationSummary,
                RestartOutcome, ServerDeploymentState, VERSION,
            },
        },
        sync::{SyncProfileMetadata, auth::User},
    },
    thunderstore::Backend,
    worker::journal::{JOURNAL_FILE, Journal},
};

const STATE_PATH: &str = "/BepInEx/config/.gale-server-state.json";
const API_DELAY: Duration = Duration::from_millis(10);

#[derive(Clone)]
struct Api {
    metadata: SyncProfileMetadata,
    archive: Arc<Vec<u8>>,
    meta_reads: Arc<AtomicUsize>,
    archive_reads: Arc<AtomicUsize>,
    archive_bytes: Arc<AtomicUsize>,
    stall: Arc<AtomicBool>,
    stalled: Arc<Notify>,
}

async fn respond(State(api): State<Api>, request: Request) -> Response {
    let path = request.uri().path();
    let auth = request.headers().get("authorization");
    match (request.method().as_str(), path) {
        ("POST", "/auth/token") => Json(json!({
            "accessToken": "eyJhbGciOiJIUzI1NiJ9.eyJleHAiOjQxMDI0NDQ4MDB9.sig",
            "refreshToken": "refresh-rotated",
        }))
        .into_response(),
        ("GET", "/profile/sync-profile-1/meta") if auth.is_some() => {
            api.meta_reads.fetch_add(1, Ordering::Relaxed);
            if api.stall.load(Ordering::Relaxed) {
                api.stalled.notify_one();
                std::future::pending::<()>().await;
            }
            tokio::time::sleep(API_DELAY).await;
            Json(api.metadata).into_response()
        }
        ("GET", "/profile/sync-profile-1") if auth.is_some() => {
            api.archive_reads.fetch_add(1, Ordering::Relaxed);
            api.archive_bytes
                .fetch_add(api.archive.len(), Ordering::Relaxed);
            tokio::time::sleep(API_DELAY).await;
            api.archive.as_ref().clone().into_response()
        }
        _ => panic!(
            "unexpected fixture request: {} {}",
            request.method(),
            request.uri()
        ),
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    ftp: Arc<FakeFtp>,
    api: Api,
    server: tokio::task::JoinHandle<()>,
    ctx: Arc<super::WorkerContext>,
    probes: Arc<AtomicUsize>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct OccupiedHost(Arc<AtomicUsize>);

impl HostControl for OccupiedHost {
    fn can_restart(&self) -> bool {
        true
    }

    fn status<'a>(&'a self) -> BoxFuture<'a, Result<HostStatus>> {
        Box::pin(async move {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(HostStatus {
                running: Some(true),
                booting: Some(false),
                players: Some(2),
            })
        })
    }

    fn restart<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { panic!("must not restart an occupied fixture") })
    }

    fn name(&self) -> &'static str {
        "occupied benchmark host"
    }
}

fn pending_record() -> OperationRecord {
    OperationRecord {
        id: "pending-restart".into(),
        executor: ExecutorKind::Worker,
        kind: OperationKind::Manual,
        worker_id: Some("fixture".into()),
        publication_revision: None,
        mods_revision: None,
        status: OperationStatus::Succeeded,
        summary: OperationSummary::default(),
        restart: RestartOutcome::AwaitingEmpty,
        error: None,
        started_at: Utc::now(),
        finished_at: Utc::now(),
    }
}

fn archive() -> Vec<u8> {
    use std::io::{Cursor, Write};
    use zip::{ZipWriter, write::SimpleFileOptions};
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    zip.start_file("export.r2x", SimpleFileOptions::default())
        .unwrap();
    zip.write_all(serde_yaml::to_string(&pack_manifest()).unwrap().as_bytes())
        .unwrap();
    zip.start_file("BepInEx/config/fixture.cfg", SimpleFileOptions::default())
        .unwrap();
    let mut seed = 7_u32;
    let config: Vec<u8> = (0..1024 * 1024)
        .map(|_| {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            b' ' + ((seed >> 24) % 95) as u8
        })
        .collect();
    zip.write_all(&config).unwrap();
    zip.finish().unwrap().into_inner()
}

async fn fixture(waiting: bool, deploy: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let ftp = Arc::new(FakeFtp::valheim_host(Options::default()));
    let metadata = SyncProfileMetadata {
        id: "sync-profile-1".into(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        owner: User {
            discord_id: "1".into(),
            name: "fixture".into(),
            display_name: "Fixture".into(),
            avatar: None,
        },
        manifest: pack_manifest(),
    };
    let api = Api {
        metadata,
        archive: Arc::new(archive()),
        meta_reads: Arc::default(),
        archive_reads: Arc::default(),
        archive_bytes: Arc::default(),
        stall: Arc::default(),
        stalled: Arc::default(),
    };
    let app = Router::new()
        .route("/auth/token", post(respond))
        .route("/profile/sync-profile-1/meta", get(respond))
        .route("/profile/sync-profile-1", get(respond))
        .with_state(api.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let journal = Journal::load(dir.path()).unwrap();
    {
        let mut state = journal.state.lock().await;
        state.auto_deploy_mods = deploy;
        if !deploy {
            state.last_deployed_revision = Some(api.metadata.updated_at);
        }
        if waiting {
            state.last_operation = Some(pending_record());
            let remote = ServerDeploymentState {
                version: VERSION,
                operation_seq: 1,
                restart_required: true,
                last_operation: state.last_operation.clone(),
                ..Default::default()
            };
            ftp.seed_file(STATE_PATH, &serde_json::to_vec(&remote).unwrap());
        }
        journal.save(&state).unwrap();
    }
    let mut config = worker_config(dir.path(), "127.0.0.1:0".into());
    config.profile_id = "sync-profile-1".into();
    config.sync_url = Some(url);
    config.remote.transport = TransportSettings {
        protocol: RemoteProtocol::Ftp,
        host: "127.0.0.1".into(),
        port: ftp.addr.port(),
        username: "u".into(),
        server_directory: "/".into(),
        ..Default::default()
    };
    let cached = crate::profile::install::cache::path(
        &dir.path().join("cache"),
        &crate::thunderstore::VersionIdent::from(("Author", "Mod", "1.0.0")),
        Backend::Thunderstore,
    );
    std::fs::create_dir_all(&cached).unwrap();
    std::fs::write(cached.join("mod.dll"), b"fixture mod").unwrap();
    let mut ctx = super::WorkerContext::new(
        config,
        crate::worker::secrets::Secrets {
            remote_password: Some("pw".into()),
            refresh_token: Some("refresh-seed".into()),
            ..worker_secrets()
        },
        journal,
    )
    .unwrap();
    let probes = Arc::new(AtomicUsize::new(0));
    ctx.host = Box::new(OccupiedHost(probes.clone()));
    let ctx = Arc::new(ctx);
    // Warm authentication outside the measurement without observing or deploying.
    ctx.sync
        .poll(&ctx.journal, Some(api.metadata.updated_at))
        .await
        .unwrap();
    Fixture {
        dir,
        ftp,
        api,
        server,
        ctx,
        probes,
    }
}

async fn poll_case(name: &str, waiting: bool, deploy: bool, cycles: usize) -> Value {
    let f = fixture(waiting, deploy).await;
    let before_reads = f.api.meta_reads.load(Ordering::Relaxed);
    let before_write = std::fs::metadata(f.dir.path().join(JOURNAL_FILE))
        .unwrap()
        .modified()
        .unwrap();
    let start = Instant::now();
    for _ in 0..cycles {
        super::poll_once(&f.ctx).await;
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let state = f.ctx.journal.state.lock().await;
    assert!(state.poll_error.is_none() && state.last_error.is_none());
    if deploy {
        assert!(state.pending.is_none());
        assert!(state.last_deployed_revision.is_some());
        let remote: ServerDeploymentState =
            serde_json::from_slice(&f.ftp.file(STATE_PATH).unwrap()).unwrap();
        assert!(remote.mods_revision.is_some());
        assert!(f.ftp.file("/BepInEx/config/fixture.cfg").is_none());
    } else if waiting {
        assert_eq!(
            state.last_operation.as_ref().unwrap().restart,
            RestartOutcome::AwaitingEmpty
        );
    }
    let commands = f.ftp.commands.lock().unwrap();
    let modified = std::fs::metadata(f.dir.path().join(JOURNAL_FILE))
        .unwrap()
        .modified()
        .unwrap();
    json!({"case":name, "ms":ms, "cycles":cycles,
        "metadata_requests":f.api.meta_reads.load(Ordering::Relaxed)-before_reads,
        "archive_requests":f.api.archive_reads.load(Ordering::Relaxed),
        "archive_bytes":f.api.archive_bytes.load(Ordering::Relaxed),
        "ftp_commands":commands.len(), "ftp_bytes":f.ftp.sent_bytes.load(Ordering::Relaxed),
        "presence_requests":f.probes.load(Ordering::Relaxed), "journal_changed":modified!=before_write})
}

async fn journal_case(root: &Path) -> Value {
    let journal = Journal::load(root).unwrap();
    let record = pending_record();
    journal.record_operation(record.clone()).await.unwrap();
    let error = eyre::eyre!("fixture sync outage");
    journal.record_poll_error(Some(&error)).await.unwrap();
    let mut changed = 0;
    let mut modified = std::fs::metadata(root.join(JOURNAL_FILE))
        .unwrap()
        .modified()
        .unwrap();
    let start = Instant::now();
    for _ in 0..100 {
        journal.record_operation(record.clone()).await.unwrap();
        let next = std::fs::metadata(root.join(JOURNAL_FILE))
            .unwrap()
            .modified()
            .unwrap();
        changed += usize::from(next != modified);
        modified = next;
        journal.record_poll_error(Some(&error)).await.unwrap();
        let next = std::fs::metadata(root.join(JOURNAL_FILE))
            .unwrap()
            .modified()
            .unwrap();
        changed += usize::from(next != modified);
        modified = next;
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let persisted = Journal::load(root).unwrap();
    assert_eq!(
        persisted
            .state
            .lock()
            .await
            .last_operation
            .as_ref()
            .unwrap()
            .id,
        record.id
    );
    json!({"case":"journal", "ms":ms, "cycles":200, "journal_rewrites":changed})
}

#[tokio::test]
#[ignore = "release benchmark; use scripts/bench-worker-poll.ps1"]
async fn poll_benchmark_workloads() {
    for (name, waiting, deploy, cycles) in [
        ("idle", false, false, 10),
        ("occupied", true, false, 5),
        ("publication", false, true, 1),
    ] {
        println!(
            "GALE_BENCH {}",
            poll_case(name, waiting, deploy, cycles).await
        );
    }
    let dir = tempfile::tempdir().unwrap();
    println!("GALE_BENCH {}", journal_case(dir.path()).await);
    let start = Instant::now();
    poll_case("idle", false, false, 10).await;
    poll_case("occupied", true, false, 5).await;
    poll_case("publication", false, true, 1).await;
    let dir = tempfile::tempdir().unwrap();
    journal_case(dir.path()).await;
    println!(
        "GALE_BENCH {}",
        json!({"case":"combined_workload", "ms":start.elapsed().as_secs_f64()*1000.0})
    );
}

#[tokio::test]
#[ignore = "virtual-time outage benchmark; use scripts/bench-worker-poll.ps1"]
async fn poll_benchmark_stalled_metadata() {
    println!("GALE_BENCH {}", stalled_case().await);
}

async fn stalled_case() -> Value {
    let f = fixture(true, false).await;
    f.api.stall.store(true, Ordering::Relaxed);
    let ctx = f.ctx.clone();
    let mut task = tokio::spawn(async move { super::poll_once(&ctx).await });
    f.api.stalled.notified().await;
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    let completed = tokio::time::timeout(Duration::from_secs(61), &mut task).await;
    let recovered = match completed {
        Ok(result) => {
            result.unwrap();
            true
        }
        Err(_) => {
            task.abort();
            let _ = task.await;
            false
        }
    };
    json!({"case":"stalled_metadata", "virtual_ms":start.elapsed().as_secs_f64()*1000.0,
        "completed":recovered, "presence_requests":f.probes.load(Ordering::Relaxed)})
}

#[tokio::test]
async fn polling_regression_occupied_restart_does_not_open_ftp() {
    let f = fixture(true, false).await;
    super::resume_deferred_restart(&f.ctx).await;
    assert!(
        f.ftp.commands.lock().unwrap().is_empty(),
        "occupied checks must not connect to FTP"
    );
    assert_eq!(f.probes.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn polling_regression_unchanged_journal_skips_writes_and_failed_changes_retry() {
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::load(dir.path()).unwrap();
    let record = pending_record();
    journal.record_operation(record.clone()).await.unwrap();
    let tmp = dir.path().join("gale-worker-state.json.tmp");
    std::fs::create_dir(&tmp).unwrap();
    journal
        .record_operation(record)
        .await
        .expect("unchanged state must not write");
    let error = eyre::eyre!("new failure");
    assert!(journal.record_poll_error(Some(&error)).await.is_err());
    std::fs::remove_dir(tmp).unwrap();
    journal.record_poll_error(Some(&error)).await.unwrap();
    let loaded = Journal::load(dir.path()).unwrap();
    assert!(
        loaded
            .state
            .lock()
            .await
            .poll_error
            .as_ref()
            .unwrap()
            .contains("new failure")
    );
    std::fs::create_dir(dir.path().join("gale-worker-state.json.tmp")).unwrap();
    journal
        .record_poll_error(Some(&error))
        .await
        .expect("identical errors must not write");
}

#[tokio::test]
async fn polling_regression_automatic_poll_downloads_publication_once() {
    let sample = poll_case("publication", false, true, 1).await;
    assert_eq!(sample["archive_requests"], 1);
}

#[tokio::test]
async fn polling_regression_stalled_sync_cannot_starve_restart_checks() {
    let sample = stalled_case().await;
    assert_eq!(
        sample["completed"], true,
        "sync poll must have a request deadline"
    );
    assert_eq!(sample["presence_requests"], 1);
}

struct RejoiningHost {
    ftp: Arc<FakeFtp>,
    probes: Arc<AtomicUsize>,
}

impl HostControl for RejoiningHost {
    fn can_restart(&self) -> bool {
        true
    }
    fn name(&self) -> &'static str {
        "rejoining fixture"
    }
    fn restart<'a>(&'a self) -> BoxFuture<'a, Result<()>> {
        Box::pin(async { panic!("a player joined before the lease was acquired") })
    }
    fn status<'a>(&'a self) -> BoxFuture<'a, Result<HostStatus>> {
        Box::pin(async move {
            let probe = self.probes.fetch_add(1, Ordering::Relaxed);
            assert_eq!(
                self.ftp.has_dir("/BepInEx/config/.gale-deploy.lock"),
                probe > 0
            );
            Ok(HostStatus {
                running: Some(true),
                booting: Some(false),
                players: Some(probe as u32),
            })
        })
    }
}

#[tokio::test]
async fn presence_is_rechecked_under_the_lease_when_a_player_joins() {
    let mut f = fixture(true, false).await;
    Arc::get_mut(&mut f.ctx).unwrap().host = Box::new(RejoiningHost {
        ftp: f.ftp.clone(),
        probes: f.probes.clone(),
    });
    super::resume_deferred_restart(&f.ctx).await;
    assert_eq!(f.probes.load(Ordering::Relaxed), 2);
    let remote: ServerDeploymentState =
        serde_json::from_slice(&f.ftp.file(STATE_PATH).unwrap()).unwrap();
    assert!(remote.restart_required);
    assert_eq!(remote.operation_seq, 1);
    assert!(!f.ftp.has_dir("/BepInEx/config/.gale-deploy.lock"));
}

#[tokio::test]
async fn a_newer_observation_supersedes_the_publication_waiting_for_the_operation_lock() {
    publication_changed_while_waiting(true).await;
}

#[tokio::test]
async fn a_new_publication_during_manual_work_is_fetched_even_before_it_is_observed() {
    publication_changed_while_waiting(false).await;
}

async fn publication_changed_while_waiting(observe: bool) {
    use crate::worker::sync_client::tests::{MockApi, serve};
    let f = fixture(false, true).await;
    let old = f.api.metadata.clone();
    let mut new = old.clone();
    new.updated_at += chrono::Duration::seconds(1);
    new.manifest.mods[0].version = semver::Version::new(2, 0, 0).into();
    let api = serve(MockApi {
        meta: Some(old),
        archive: super::tests::publication_zip(&pack_manifest()),
        // fixture() already rotated the seed in this journal.
        refresh_tokens: Arc::new(tokio::sync::Mutex::new(vec!["refresh-seed".into()])),
        ..Default::default()
    })
    .await;
    let mut config = f.ctx.config.clone();
    config.sync_url = Some(api.url.clone());
    let ctx = Arc::new(
        super::WorkerContext::new(
            config,
            crate::worker::secrets::Secrets {
                remote_password: Some("pw".into()),
                refresh_token: Some("refresh-seed".into()),
                ..worker_secrets()
            },
            Journal::load(f.dir.path()).unwrap(),
        )
        .unwrap(),
    );
    let old_publication = super::observe_publication(&ctx).await.unwrap();
    let guard = ctx.operation_lock.lock().await;
    let mut deploy = Box::pin(super::deploy_pending(&ctx, old_publication));
    assert!(futures_util::poll!(deploy.as_mut()).is_pending());
    api.publish(new.clone(), super::tests::publication_zip(&new.manifest));
    if observe {
        super::observe_publication(&ctx).await.unwrap();
    }
    let cached = crate::profile::install::cache::path(
        &f.dir.path().join("cache"),
        &crate::thunderstore::VersionIdent::from(("Author", "Mod", "2.0.0")),
        Backend::Thunderstore,
    );
    std::fs::create_dir_all(cached.join("BepInEx/plugins/Author-Mod")).unwrap();
    std::fs::write(
        cached.join("BepInEx/plugins/Author-Mod/mod.dll"),
        b"version 2",
    )
    .unwrap();
    drop(guard);
    deploy.await;
    let state = ctx.journal.state.lock().await;
    assert!(state.last_error.is_none(), "{:?}", state.last_error);
    assert_eq!(state.last_deployed_revision, Some(new.updated_at));
    assert!(state.pending.is_none());
    assert_eq!(
        f.ftp.file("/BepInEx/plugins/Author-Mod/mod.dll").unwrap(),
        b"version 2"
    );
}

#[tokio::test]
async fn archive_deadline_covers_a_stalled_response_body() {
    use crate::worker::sync_client::SyncClient;
    use futures_util::StreamExt;
    async fn stalled_body(State(api): State<Api>) -> Response {
        api.stalled.notify_one();
        let first = futures_util::stream::once(async {
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"PK"))
        });
        let body = first.chain(futures_util::stream::pending());
        axum::body::Body::from_stream(body).into_response()
    }
    let mut f = fixture(false, false).await;
    let app = Router::new()
        .route("/auth/token", post(respond))
        .route("/profile/sync-profile-1/meta", get(respond))
        .route("/profile/sync-profile-1", get(stalled_body))
        .with_state(f.api.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = f.ctx.config.clone();
    config.sync_url = Some(format!("http://{}", listener.local_addr().unwrap()));
    f.server.abort();
    f.server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = SyncClient::new(config, Some("refresh-seed".into()));
    let journal = f.ctx.journal.clone();
    let mut task = tokio::spawn(async move { client.poll(&journal, None).await });
    f.api.stalled.notified().await;
    tokio::time::pause();
    let result = tokio::time::timeout(Duration::from_secs(121), &mut task).await;
    let error = match result {
        Ok(joined) => match joined.unwrap() {
            Ok(_) => panic!("a partial ZIP body cannot succeed"),
            Err(error) => error,
        },
        Err(_) => {
            task.abort();
            let _ = task.await;
            panic!("archive body outlived its deadline")
        }
    };
    assert!(
        error.chain().any(|cause| cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(reqwest::Error::is_timeout)),
        "{error:#}"
    );
}

#[tokio::test]
async fn failed_journal_replace_retries_with_the_rotated_credential() {
    use crate::worker::sync_client::{
        SyncClient,
        tests::{MockApi, serve},
    };
    let dir = tempfile::tempdir().unwrap();
    let journal = Arc::new(Journal::load(dir.path()).unwrap());
    // Fail at rename, after the temp bytes have already been flushed.
    let target = dir.path().join(JOURNAL_FILE);
    std::fs::create_dir(&target).unwrap();
    let api = MockApi::default();
    let server = serve(api.clone()).await;
    let mut config = worker_config(dir.path(), "127.0.0.1:0".into());
    config.profile_id = "sync-profile-1".into();
    config.sync_url = Some(server.url.clone());
    let client = SyncClient::new(config, Some("refresh-seed".into()));
    let error = client
        .poll(&journal, None)
        .await
        .err()
        .expect("rename must fail");
    assert!(format!("{error:#}").contains("could not save"));
    std::fs::remove_dir(&target).unwrap();
    assert!(client.poll(&journal, None).await.unwrap().is_none());
    // A failed save discards the access token. After recommitting, the
    // client may grant again with the rotated credential, never the seed.
    assert_eq!(
        *api.refresh_tokens.lock().await,
        ["refresh-seed", "refresh-rotated"]
    );
    let persisted = Journal::load(dir.path()).unwrap();
    assert_eq!(
        persisted.state.lock().await.refresh_token.as_deref(),
        Some("refresh-rotated")
    );
}
