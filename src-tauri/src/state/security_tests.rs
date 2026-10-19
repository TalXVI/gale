//! Native acceptance with a hidden WebView2 window and isolated application state.
// tauri-build embeds Common Controls v6 only in application binaries, not libtest.
#[cfg(target_env = "msvc")]
#[used]
#[unsafe(link_section = ".drectve")]
static COMMON_CONTROLS: [u8; COMMON_CONTROLS_BYTES.len()] = *COMMON_CONTROLS_BYTES;
#[cfg(target_env = "msvc")]
const COMMON_CONTROLS_BYTES: &[u8; 168] = b" /MANIFESTDEPENDENCY:\"type='win32' name='Microsoft.Windows.Common-Controls' version='6.0.0.0' processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'\" ";
use super::*;
use crate::{
    config::ConfigCache,
    profile::{
        LocalMod, ManagedGame, Profile, ProfileMod,
        sync::{archive, auth},
    },
    test_support::CredentialStore,
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::State,
    http::{Method, Request, StatusCode},
    response::IntoResponse,
    routing::any,
};
use base64::{Engine, prelude::BASE64_URL_SAFE_NO_PAD};
use futures_util::FutureExt;
use serde_json::{Value, json};
use std::{
    borrow::Cow,
    collections::HashMap,
    sync::atomic::{AtomicUsize, Ordering},
};
use tauri::{
    Assets, WebviewUrl, WebviewWindowBuilder,
    utils::assets::{AssetKey, AssetsIter, CspHash},
};

struct FixtureAssets;
impl Assets<tauri::Wry> for FixtureAssets {
    fn get(&self, _: &AssetKey) -> Option<Cow<'_, [u8]>> {
        Some(Cow::Borrowed(
            b"<!doctype html><title>Gale security fixture</title>",
        ))
    }
    fn iter(&self) -> Box<AssetsIter<'_>> {
        Box::new(std::iter::empty())
    }
    fn csp_hashes(&self, _: &AssetKey) -> Box<dyn Iterator<Item = CspHash<'_>> + '_> {
        Box::new(std::iter::empty())
    }
}

type AssetReads = Vec<(u16, String)>;

#[derive(Default)]
struct AssetReply(Mutex<Option<tokio::sync::oneshot::Sender<AssetReads>>>);

#[tauri::command]
fn asset_reply(results: AssetReads, reply: tauri::State<'_, AssetReply>) {
    reply
        .0
        .lock()
        .unwrap()
        .take()
        .unwrap()
        .send(results)
        .unwrap();
}

async fn read_assets(app: &AppHandle, paths: &[std::path::PathBuf]) -> AssetReads {
    let (tx, rx) = tokio::sync::oneshot::channel();
    *app.state::<AssetReply>().0.lock().unwrap() = Some(tx);
    app.get_webview_window("main").unwrap().eval(format!(
        "Promise.all({}.map(async path => {{ const r = await fetch('http://asset.localhost/' + encodeURIComponent(path)); return [r.status, await r.text()]; }})).then(results => window.__TAURI_INTERNALS__.invoke('asset_reply', {{results}}))",
        serde_json::to_string(paths).unwrap()
    )).unwrap();
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .unwrap()
        .unwrap()
}

#[derive(Default)]
struct Api {
    uploads: Mutex<Vec<Vec<u8>>>,
    grants: AtomicUsize,
    unexpected: AtomicUsize,
}

fn user() -> Value {
    json!({"discordId":"fixture", "name":"Fixture", "displayName":"Fixture", "avatar":null})
}
fn access_token() -> String {
    let mut payload = user();
    payload["exp"] = json!(4000000000i64);
    format!(
        "header.{}.signature",
        BASE64_URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
    )
}

async fn api(
    State(api): State<Arc<Api>>,
    request: Request<axum::body::Body>,
) -> axum::response::Response {
    let response = std::panic::AssertUnwindSafe(async {
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let body = to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
        match (method, path.as_str()) {
            (Method::POST, "/auth/token") => {
                api.grants.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    serde_json::from_slice::<Value>(&body).unwrap()["refreshToken"],
                    "spent-refresh"
                );
                Json(json!({"accessToken":access_token(), "refreshToken":"rotated-refresh"}))
                    .into_response()
            }
            (Method::POST, "/profile") | (Method::PUT, "/profile/fixture") => {
                archive::validate(&body).unwrap();
                api.uploads.lock().unwrap().push(body.to_vec());
                Json(json!({"id":"fixture", "createdAt":"2026-01-01T00:00:00Z", "updatedAt":"2026-01-01T00:00:00Z"})).into_response()
            }
            (Method::GET, "/profile/fixture/meta") => {
                let uploads = api.uploads.lock().unwrap();
                let archive = archive::validate(uploads.last().unwrap()).unwrap();
                Json(json!({"id":"fixture", "createdAt":"2026-01-01T00:00:00Z", "updatedAt":"2026-01-01T00:00:00Z", "owner":user(), "manifest":archive.manifest})).into_response()
            }
            _ => {
                api.unexpected.fetch_add(1, Ordering::SeqCst);
                StatusCode::NOT_FOUND.into_response()
            }
        }
    })
    .catch_unwind()
    .await;
    match response {
        Ok(response) => response,
        Err(_) => {
            // A handler panic must fail acceptance instead of becoming an expected client error.
            api.unexpected.fetch_add(1, Ordering::SeqCst);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

struct AbortServer(tokio::task::JoinHandle<()>);
impl Drop for AbortServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, Copy, Debug)]
enum Case {
    Rotation,
    Assets,
    Import,
    Publication,
}
impl Case {
    const ALL: [Self; 4] = [
        Self::Rotation,
        Self::Assets,
        Self::Import,
        Self::Publication,
    ];

    async fn run(self, app: AppHandle, root: &std::path::Path) {
        std::fs::create_dir_all(root).unwrap();
        let fixture = fixture(&app, root, self).await;
        match self {
            Self::Rotation => rotation(app, &fixture).await,
            Self::Assets => assets(app, root).await,
            Self::Import => import(app, root).await,
            Self::Publication => publication(app, &fixture).await,
        }
        assert_eq!(fixture.api.unexpected.load(Ordering::SeqCst), 0);
    }
}

struct Fixture {
    store: CredentialStore,
    api: Arc<Api>,
    _server: AbortServer,
}

async fn fixture(app: &AppHandle, root: &std::path::Path, case: Case) -> Fixture {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let api_state = Arc::new(Api::default());
    let router = Router::new()
        .fallback(any(api))
        .with_state(api_state.clone());
    let server = AbortServer(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));
    let http = reqwest_middleware::ClientBuilder::new(
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(proxy).unwrap())
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
    )
    .build();
    let mut conn = rusqlite::Connection::open(root.join("data.sqlite3")).unwrap();
    static MIGRATIONS: include_dir::Dir =
        include_dir::include_dir!("$CARGO_MANIFEST_DIR/migrations");
    rusqlite_migration::Migrations::from_directory(&MIGRATIONS)
        .unwrap()
        .to_latest(&mut conn)
        .unwrap();
    let store = CredentialStore::default();
    let db = Db::new(conn, store.entry());
    let credentials = match case {
        Case::Rotation => {
            json!({"user":user(), "accessToken":"expired", "tokenExpiry":0, "refreshToken":"spent-refresh"})
        }
        _ => {
            json!({"user":user(), "accessToken":access_token(), "tokenExpiry":4000000000i64, "refreshToken":"rotated-refresh"})
        }
    };
    let old = serde_json::from_value(credentials).unwrap();
    db.save_auth(Some(&old)).unwrap();
    let game = crate::game::from_slug("among-us").unwrap();
    let path = root.join("profile");
    std::fs::create_dir_all(path.join("BepInEx/config")).unwrap();
    std::fs::write(path.join("BepInEx/config/private.cfg"), b"private-value").unwrap();
    std::fs::write(path.join("BepInEx/config/shared.cfg"), b"shared-value").unwrap();
    let icon = path.join("BepInEx/plugins/Fixture/icon.png");
    std::fs::create_dir_all(icon.parent().unwrap()).unwrap();
    std::fs::write(&icon, b"fixture-icon").unwrap();
    std::fs::write(icon.with_extension("png.old"), b"fixture-disabled-icon").unwrap();
    let outside = root.join("outside/icon.png");
    std::fs::create_dir_all(outside.parent().unwrap()).unwrap();
    std::fs::write(&outside, b"outside-value").unwrap();
    let profile = Profile {
        id: 1,
        name: "Fixture".into(),
        path: path.clone(),
        game,
        mods: [("Fixture", icon.clone()), ("Outside", outside.clone())]
            .into_iter()
            .map(|(name, icon)| ProfileMod {
                enabled: true,
                install_time: chrono::Utc::now(),
                kind: crate::profile::ProfileModKind::Local(Box::new(LocalMod {
                    name: name.into(),
                    icon: Some(icon),
                    ..Default::default()
                })),
            })
            .collect(),
        ignored_version_updates: Default::default(),
        ignored_package_updates: Default::default(),
        config_cache: ConfigCache::default(),
        linked_config: Default::default(),
        modpack: None,
        sync: None,
        custom_args: String::new(),
        server_settings: None,
        missing: false,
        excluded_export_files: Default::default(),
    };
    let manager = ModManager {
        games: HashMap::from([(
            game,
            ManagedGame {
                id: 1,
                game,
                path: path.clone(),
                profiles: vec![profile],
                favorite: false,
                active_profile_id: 1,
            },
        )]),
        active_game: game,
        hidden_mods: Default::default(),
    };
    db.save_all(&manager).unwrap();
    app.manage(AppState {
        http,
        prefs: Mutex::new(Prefs {
            data_dir: root.to_path_buf().into(),
            ..Default::default()
        }),
        manager: Mutex::new(manager),
        thunderstore: Mutex::new(Thunderstore::new()),
        db,
        install_queue: InstallQueue::new(app.clone()),
        sync_auth: auth::State::new(Some(old)),
        sync_socket: sync::socket::State::new(app.clone()),
        event_buffer: EventBuffer::new(app.clone()),
        is_first_run: true,
        server_runtime: Mutex::new(ServerRuntime::default()),
    });

    Fixture {
        store,
        api: api_state,
        _server: server,
    }
}

async fn rotation(app: AppHandle, fixture: &Fixture) {
    // A spent token must never be granted again while its replacement cannot save.
    fixture.store.block_writes(true);
    assert!(auth::access_token(&app).await.is_err());
    assert_eq!(fixture.api.grants.load(Ordering::SeqCst), 1);
    assert!(auth::access_token(&app).await.is_err());
    assert!(auth::verify_session(&app).await.is_err());
    assert_eq!(fixture.api.grants.load(Ordering::SeqCst), 1);
    fixture.store.block_writes(false);
    assert_eq!(
        auth::access_token(&app).await.unwrap().unwrap(),
        access_token()
    );
    assert!(
        fixture
            .store
            .entry()
            .get_password()
            .unwrap()
            .contains("rotated-refresh")
    );
    assert_eq!(fixture.api.grants.load(Ordering::SeqCst), 1);
}

async fn assets(app: AppHandle, root: &std::path::Path) {
    let path = root.join("profile");
    let icon = path.join("BepInEx/plugins/Fixture/icon.png");
    let outside = root.join("outside/icon.png");
    // The real configured asset protocol denies everything before query_profile.
    let paths = [
        icon.clone(),
        icon.with_extension("png.old"),
        path.join("BepInEx/config/private.cfg"),
        outside,
    ];
    assert!(
        read_assets(&app, &paths)
            .await
            .iter()
            .all(|(status, _)| *status == 403)
    );
    let query = serde_json::from_value(json!({"maxCount":null, "searchTerm":null, "includeCategories":[], "excludeCategories":[], "includeNsfw":true, "includeDeprecated":true, "includeDisabled":true, "includeEnabled":true, "sortBy":"name", "sortOrder":"ascending"})).unwrap();
    crate::profile::commands::query_profile(query, app.clone()).unwrap();
    let reads = read_assets(&app, &paths).await;
    assert_eq!(reads[0], (200, "fixture-icon".into()));
    assert_eq!(reads[1], (200, "fixture-disabled-icon".into()));
    assert_eq!(reads[2].0, 403);
    assert_eq!(reads[3].0, 403);
}

async fn import(app: AppHandle, root: &std::path::Path) {
    // Invalid manifests must return before acquiring the dependency/manager lock.
    use std::io::Write;
    let zip_path = root.join("invalid.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&zip_path).unwrap());
    zip.start_file("manifest.json", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(br#"{"name":"../outside","version_number":"1.0.0","description":"Fixture","website_url":"","dependencies":["Example-Dependency-1.0.0"]}"#).unwrap();
    zip.finish().unwrap();
    {
        let manager = app.lock_manager();
        let count = manager.active_profile().mods.len();
        let (tx, rx) = std::sync::mpsc::channel();
        let imported_app = app.clone();
        let import = std::thread::spawn(move || {
            tx.send(tauri::async_runtime::block_on(
                crate::profile::import::import_local_mod(
                    zip_path,
                    None,
                    &imported_app,
                    Default::default(),
                ),
            ))
            .unwrap()
        });
        let rejected = rx.recv_timeout(Duration::from_secs(2));
        assert_eq!(manager.active_profile().mods.len(), count);
        drop(manager);
        import.join().unwrap();
        assert!(
            rejected
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("single filesystem component")
        );
    }
}

async fn publication(app: AppHandle, fixture: &Fixture) {
    // Publish once, then exclude an old snapshot file and publish mods only.
    sync::commands::create_sync_profile(1, app.clone())
        .await
        .unwrap();
    assert_eq!(
        archive::validate(&fixture.api.uploads.lock().unwrap()[0])
            .unwrap()
            .config
            .len(),
        2
    );
    app.lock_manager()
        .active_profile_mut()
        .excluded_export_files
        .insert("BepInEx/config/private.cfg".into());
    let mods = serde_json::from_value(json!({"kind":"mods"})).unwrap();
    sync::commands::push_sync_profile(mods, 1, app.clone())
        .await
        .unwrap();
    {
        let uploads = fixture.api.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 2);
        let published = archive::validate(&uploads[1]).unwrap();
        assert_eq!(published.config.len(), 1);
        assert_eq!(
            published.config.values().next().unwrap().bytes,
            b"shared-value"
        );
    }
    let selected =
        serde_json::from_value(json!({"kind":"config", "files":["BepInEx/config/private.cfg"]}))
            .unwrap();
    assert!(
        sync::commands::push_sync_profile(selected, 1, app.clone())
            .await
            .is_err()
    );
    assert_eq!(fixture.api.uploads.lock().unwrap().len(), 2);
}

#[test]
fn security_application_boundaries_use_real_state_and_asset_delivery() {
    assert_eq!(
        &*sync::API_URL,
        "http://127.0.0.1:1",
        "native fixtures require the documented isolated sync URL"
    );
    let root = tempfile::tempdir().unwrap();
    let mut context = tauri::generate_context!();
    context.config_mut().app.windows.clear();
    context.config_mut().build.dev_url = None;
    context.set_assets(Box::new(FixtureAssets));
    let webview_dir = root.path().join("webview");
    let (loaded_tx, loaded_rx) = tokio::sync::oneshot::channel();
    let loaded_tx = Arc::new(Mutex::new(Some(loaded_tx)));
    let app = tauri::Builder::default()
        .any_thread()
        .manage(AssetReply::default())
        .invoke_handler(tauri::generate_handler![asset_reply])
        .setup(move |app| {
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .visible(false)
                .data_directory(webview_dir)
                .on_page_load(move |_, payload| {
                    if payload.event() == tauri::webview::PageLoadEvent::Finished
                        && let Some(tx) = loaded_tx.lock().unwrap().take()
                    {
                        tx.send(()).unwrap();
                    }
                })
                .build()?;
            Ok(())
        })
        .build(context)
        .unwrap();
    let (result_tx, result_rx) = std::sync::mpsc::channel();
    let mut ready = Some((loaded_rx, root.path().to_path_buf(), result_tx));
    app.run_return(move |app, event| {
        if matches!(event, tauri::RunEvent::Ready) {
            let (loaded, root, result) = ready.take().unwrap();
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let outcome = std::panic::AssertUnwindSafe(async {
                    tokio::time::timeout(Duration::from_secs(15), loaded)
                        .await
                        .unwrap()
                        .unwrap();
                    let mut failures = Vec::new();
                    for case in Case::ALL {
                        let name = format!("{case:?}");
                        let outcome = std::panic::AssertUnwindSafe(async {
                            tokio::time::timeout(
                                Duration::from_secs(30),
                                case.run(app.clone(), &root.join(&name)),
                            )
                            .await
                            .unwrap();
                        })
                        .catch_unwind()
                        .await;
                        let cleanup = std::panic::AssertUnwindSafe(async {
                            if let Some(state) = app.try_state::<AppState>() {
                                let queue = Arc::downgrade(&state.install_queue);
                                state.install_queue.shutdown().await.unwrap();
                                #[expect(
                                    deprecated,
                                    reason = "Fixture operations and queue shutdown have finished."
                                )]
                                let _ = app.unmanage::<AppState>();
                                assert!(
                                    queue.upgrade().is_none(),
                                    "fixture retained its install queue"
                                );
                            }
                        })
                        .catch_unwind()
                        .await;
                        match outcome.and(cleanup) {
                            Ok(()) => eprintln!("native security {name}: passed"),
                            Err(panic) => {
                                eprintln!("native security {name}: failed");
                                failures.push(panic);
                            }
                        }
                    }
                    if let Some(panic) = failures.into_iter().next() {
                        std::panic::resume_unwind(panic);
                    }
                })
                .catch_unwind()
                .await;
                result.send(outcome).unwrap();
                app.exit(0);
            });
        }
    });
    if let Err(panic) = result_rx.recv_timeout(Duration::from_secs(150)).unwrap() {
        std::panic::resume_unwind(panic);
    }
}
