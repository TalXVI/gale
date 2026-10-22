//! Opt-in local benchmarks of the service used by desktop and Worker requests.

use std::time::{Duration, Instant};

use super::*;
use crate::profile::{
    install::cache,
    server::{
        progress::SyncPhase,
        remote::fake_ftp::{FakeFtp, Options},
        service::{Connector, DeployService, PinAdvice},
    },
};

static LOADER: ModLoader<'static> = ModLoader {
    package_name: None,
    file_target: None,
    kind: ModLoaderKind::BepInEx {
        extra_subdirs: Vec::new(),
    },
};

struct Fixture {
    service: DeployService,
    server: FakeFtp,
    mirror_root: bool,
    _cache: tempfile::TempDir,
}

impl Fixture {
    fn new(delay: Duration, tls: bool, mirror_root: bool) -> Self {
        let options = Options {
            tls,
            command_delay: delay,
            size_requires_binary: true,
            ..Default::default()
        };
        let server = if mirror_root {
            FakeFtp::spawn(options)
        } else {
            FakeFtp::valheim_host(options)
        };
        let loader_root = if mirror_root { "" } else { "/BepInEx" };
        server.seed_dir(&format!("{loader_root}/plugins"));
        server.seed_dir(&format!("{loader_root}/config"));
        // Foreign payloads and runtime configs must survive every operation.
        server.seed_dir(&format!("{loader_root}/plugins/Foreign"));
        server.seed_file(
            &format!("{loader_root}/plugins/Foreign/keep.dll"),
            b"foreign",
        );
        server.seed_file(
            &format!("{loader_root}/config/runtime.cfg"),
            b"server-authoritative",
        );
        let cache = tempfile::tempdir().unwrap();
        let service = DeployService {
            connector: Connector {
                settings: TransportSettings {
                    protocol: if tls {
                        RemoteProtocol::Ftps
                    } else {
                        RemoteProtocol::Ftp
                    },
                    host: "127.0.0.1".to_owned(),
                    port: server.addr.port(),
                    username: "fixture".to_owned(),
                    server_directory: "/".to_owned(),
                    trusted_certificate: server.trusted_certificate(),
                    ..Default::default()
                },
                password: "fixture".to_owned(),
                pin_advice: PinAdvice {
                    host_key: "fixture",
                    certificate: "fixture",
                },
            },
            spec: spec(),
            profile_id: "benchmark".to_owned(),
            game: "valheim".to_owned(),
            mod_loader: &LOADER,
            cache_dir: cache.path().to_path_buf(),
            // No request is allowed to escape this fixture, even on a cache miss.
            http: reqwest_middleware::ClientBuilder::new(
                reqwest::Client::builder()
                    .proxy(reqwest::Proxy::all("http://127.0.0.1:1").unwrap())
                    .build()
                    .unwrap(),
            )
            .build(),
        };
        Self {
            service,
            server,
            mirror_root,
            _cache: cache,
        }
    }

    fn remote_path(&self, path: &str) -> String {
        format!(
            "/{}",
            if self.mirror_root {
                path.strip_prefix("BepInEx/").unwrap()
            } else {
                path
            }
        )
    }

    fn publication(&self, mods: usize, updated: bool) -> FetchedPublication {
        let mut publication = mod_fixture().publication();
        publication.manifest.mods.clear();
        for index in 0..mods {
            let r2mod = R2Mod {
                ident: PackageIdent::from(("Bench", format!("Mod{index:03}").as_str())),
                version: semver::Version::new(1, u64::from(updated && index % 8 == 0), 0).into(),
                enabled: true,
                source: Backend::Thunderstore,
            };
            let ident = r2mod.version_ident();
            let root = cache::path(&self.service.cache_dir, &ident, r2mod.source)
                .join("BepInEx/plugins")
                .join(ident.full_name());
            std::fs::create_dir_all(root.join("translations")).unwrap();
            for (name, size) in [
                ("Mod.dll", 128 * 1024 + 1),
                ("manifest.json", 1024),
                ("translations/en.json", 8193),
                ("translations/de.json", 4096),
            ] {
                let mut bytes = vec![(index % 251) as u8; size];
                bytes[0] = u8::from(updated && index % 8 == 0);
                std::fs::write(root.join(name), bytes).unwrap();
            }
            publication.manifest.mods.push(r2mod);
        }
        publication.mods_revision =
            ModRevision::from_hash(blake3::hash(format!("{mods}-{updated}").as_bytes()));
        publication
    }

    async fn pair(
        &self,
        case: &str,
        publication: &FetchedPublication,
        delay_ms: u64,
        protocol: &str,
        expected_uploads: usize,
        expected_removals: usize,
    ) {
        let selection = selection(true, false);
        let preview_publication = publication.clone();
        let deploy_publication = publication.clone();
        let (mut progress, phases, started) = self.measure(SyncOperation::Preview, &selection);
        let preview = self
            .service
            .preview(
                preview_publication,
                selection.clone(),
                RestartPolicy::Manual,
                meta(),
                &mut progress,
            )
            .await
            .unwrap();
        progress.succeeded();
        self.report(
            &format!("preview_{case}"),
            delay_ms,
            protocol,
            started,
            &phases,
        );
        assert!(preview.busy.is_none());
        assert_eq!(preview.plan.uploads.len(), expected_uploads);
        assert_eq!(preview.plan.removals.len(), expected_removals);
        assert_eq!(
            preview.plan.unmanaged,
            vec![deploy_path("BepInEx/plugins/Foreign/keep.dll")]
        );

        let (mut progress, phases, started) = self.measure(SyncOperation::Deploy, &selection);
        let host = host::from_settings(&Default::default(), None);
        let deployed = self
            .service
            .deploy(
                deploy_publication,
                selection,
                Some(preview.plan.hash),
                false,
                RestartPolicy::Manual,
                host.as_ref(),
                meta(),
                &mut progress,
            )
            .await
            .unwrap();
        self.report(
            &format!("deploy_{case}"),
            delay_ms,
            protocol,
            started,
            &phases,
        );
        assert_eq!(deployed.summary.uploaded_files, expected_uploads);
        assert_eq!(deployed.summary.removed_files, expected_removals);
        assert!(deployed.warnings.is_empty(), "{:?}", deployed.warnings);
        assert_eq!(
            self.server
                .file(&self.remote_path("BepInEx/plugins/Foreign/keep.dll"))
                .unwrap(),
            b"foreign"
        );
        assert_eq!(
            self.server
                .file(&self.remote_path("BepInEx/config/runtime.cfg"))
                .unwrap(),
            b"server-authoritative"
        );
        let state: ServerDeploymentState = serde_json::from_slice(
            &self
                .server
                .file(&self.remote_path("BepInEx/config/.gale-server-state.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(state.files.len(), publication.manifest.mods.len() * 4);
        assert_eq!(state.mods_revision, Some(publication.mods_revision.clone()));
        assert!(state.config.is_empty());
        assert!(
            !self
                .server
                .has_dir(&self.remote_path("BepInEx/config/.gale-deploy.lock"))
        );
        if self.mirror_root {
            assert!(!self.server.has_dir("/BepInEx"));
        }
        // Verify the final bytes independently of the client's state record.
        for r2mod in &publication.manifest.mods {
            let ident = r2mod.version_ident();
            let cached = cache::path(&self.service.cache_dir, &ident, r2mod.source);
            for file in [
                "Mod.dll",
                "manifest.json",
                "translations/en.json",
                "translations/de.json",
            ] {
                let path = format!("BepInEx/plugins/{}/{file}", ident.full_name());
                assert_eq!(
                    self.server.file(&self.remote_path(&path)).unwrap(),
                    std::fs::read(cached.join(path)).unwrap()
                );
            }
        }
    }

    fn measure(
        &self,
        operation: SyncOperation,
        selection: &DeploySelection,
    ) -> (ProgressReporter, Arc<Mutex<Phases>>, Instant) {
        self.server.reset_peak_connections();
        self.server.clear_commands();
        self.server
            .sent_bytes
            .store(0, std::sync::atomic::Ordering::SeqCst);
        let phases = Arc::new(Mutex::new(Phases {
            last: Instant::now(),
            phase: SyncPhase::FetchingPublication,
            milliseconds: BTreeMap::new(),
        }));
        let sink = phases.clone();
        let started = Instant::now();
        let progress =
            ProgressReporter::new("benchmark".to_owned(), operation, selection, move |event| {
                let mut phases = sink.lock().unwrap();
                if phases.phase != event.phase
                    || event.status == crate::profile::server::progress::ProgressStatus::Succeeded
                {
                    let elapsed = phases.last.elapsed().as_secs_f64() * 1000.0;
                    let key = serde_json::to_value(phases.phase)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .to_owned();
                    *phases.milliseconds.entry(key).or_default() += elapsed;
                    phases.last = Instant::now();
                    phases.phase = event.phase;
                }
            });
        (progress, phases, started)
    }

    fn report(
        &self,
        case: &str,
        delay_ms: u64,
        protocol: &str,
        started: Instant,
        phases: &Mutex<Phases>,
    ) {
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        let mut commands = BTreeMap::<String, usize>::new();
        for command in self.server.commands.lock().unwrap().iter() {
            *commands
                .entry(command.split(' ').next().unwrap().to_owned())
                .or_default() += 1;
            assert!(
                !command.contains("runtime.cfg"),
                "mods operation touched runtime config: {command}"
            );
        }
        println!(
            "GALE_DEPLOY_BENCH {}",
            serde_json::json!({
                "case": case, "protocol": protocol, "command_delay_ms": delay_ms, "ms": ms,
                "layout": if self.mirror_root { "MirrorRoot" } else { "Standard" },
                "phases_ms": phases.lock().unwrap().milliseconds,
                "commands": commands, "ftp_commands": commands.values().sum::<usize>(),
                "download_bytes": self.server.sent_bytes.load(std::sync::atomic::Ordering::SeqCst),
                "peak_connections": self.server.peak_connections(),
            })
        );
    }
}

struct Phases {
    last: Instant,
    phase: SyncPhase,
    milliseconds: BTreeMap<String, f64>,
}

#[tokio::test]
#[ignore = "local performance benchmark; run scripts/bench-server-sync.ps1"]
async fn server_sync_benchmark() {
    let delay_ms = std::env::var("GALE_BENCH_COMMAND_DELAY_MS")
        .map(|value| value.parse::<u64>().unwrap())
        .unwrap_or(10);
    let mods = std::env::var("GALE_BENCH_MODS")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(32);
    assert!(mods >= 8);
    let protocol = std::env::var("GALE_BENCH_PROTOCOL").unwrap_or_else(|_| "ftps".to_owned());
    assert!(matches!(protocol.as_str(), "ftp" | "ftps"));
    let layout = std::env::var("GALE_BENCH_LAYOUT").unwrap_or_else(|_| "Standard".to_owned());
    assert!(matches!(layout.as_str(), "Standard" | "MirrorRoot"));
    let fixture = Fixture::new(
        Duration::from_millis(delay_ms),
        protocol == "ftps",
        layout == "MirrorRoot",
    );
    let publication = fixture.publication(mods, false);
    fixture
        .pair("first", &publication, delay_ms, &protocol, mods * 4, 0)
        .await;
    fixture
        .pair("noop", &publication, delay_ms, &protocol, 0, 0)
        .await;
    let updated = fixture.publication(mods - 2, true);
    fixture
        .pair(
            "delta",
            &updated,
            delay_ms,
            &protocol,
            (mods - 2).div_ceil(8) * 4,
            8,
        )
        .await;
    // Same-size remote drift must still invalidate a preview and trigger repair.
    let path = fixture.remote_path("BepInEx/plugins/Bench-Mod001/Mod.dll");
    let mut bytes = fixture.server.file(&path).unwrap();
    bytes[0] ^= 1;
    fixture.server.seed_file(&path, &bytes);
    fixture
        .pair("drift", &updated, delay_ms, &protocol, 1, 0)
        .await;
    fixture
        .pair(
            "clear",
            &fixture.publication(0, false),
            delay_ms,
            &protocol,
            0,
            (mods - 2) * 4,
        )
        .await;
    benchmark_package_waits(&fixture, &publication, delay_ms, &protocol).await;
}

/// Models package availability with local cache trees and a fixed wait per package.
/// Remote service cases above include real cache lookup and hashing without this wait.
struct DelayedSource {
    root: std::path::PathBuf,
    active: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
}

impl crate::profile::server::stage::PayloadSource for DelayedSource {
    fn stage<'a>(
        &'a self,
        ident: &'a crate::thunderstore::VersionIdent,
        backend: Backend,
    ) -> futures_util::future::BoxFuture<'a, Result<std::path::PathBuf>> {
        Box::pin(async move {
            use std::sync::atomic::Ordering;
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(active, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(100)).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(cache::path(&self.root, ident, backend))
        })
    }
}

async fn benchmark_package_waits(
    fixture: &Fixture,
    publication: &FetchedPublication,
    delay_ms: u64,
    protocol: &str,
) {
    let source = DelayedSource {
        root: fixture.service.cache_dir.clone(),
        active: Default::default(),
        peak: Default::default(),
    };
    let started = Instant::now();
    let desired = crate::profile::server::stage::stage_publication(
        publication,
        &source,
        &fixture.service.spec,
        true,
        |_, _, _| {},
    )
    .await
    .unwrap();
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    assert_eq!(desired.len(), publication.manifest.mods.len() * 4);
    assert_eq!(source.active.load(std::sync::atomic::Ordering::SeqCst), 0);
    println!(
        "GALE_DEPLOY_BENCH {}",
        serde_json::json!({
            "case": "stage_package_waits", "protocol": protocol, "command_delay_ms": delay_ms,
            "package_delay_ms": 100, "ms": ms, "ftp_commands": 0,
            "download_bytes": 0, "peak_connections": 0,
            "peak_package_requests": source.peak.load(std::sync::atomic::Ordering::SeqCst),
        })
    );
}
