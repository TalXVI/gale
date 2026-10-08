use std::collections::HashMap;

use chrono::{DateTime, Utc};
use eyre::{Context, Result, bail, ensure, eyre};
use reqwest_middleware::ClientWithMiddleware;
use tauri::AppHandle;
use uuid::Uuid;

use crate::{
    game::Game,
    profile::export::R2Mod,
    state::ManagerExt,
    thunderstore::{
        self, Backend, FromBackend, PackageIdent, PackageListing, Thunderstore, VersionIdent,
    },
};

struct RequiredPackage {
    uuid: Uuid,
    ident: PackageIdent,
    versions: Vec<VersionIdent>,
    url: String,
}

fn required_packages(
    mods: &[R2Mod],
    state: &Thunderstore,
    game: Game,
) -> Result<Vec<RequiredPackage>> {
    let mut packages = Vec::<RequiredPackage>::new();
    let mut indices = HashMap::<Uuid, usize>::new();
    for entry in mods {
        if entry.to_install(state).is_ok() || entry.source != Backend::Thunderstore {
            continue;
        }
        let version = entry.version_ident();
        let package = state
            .find_package(entry.ident.as_str(), Backend::Thunderstore)
            .with_context(|| format!("Published mod {version} is not available in the catalog"))?;
        if let Some(&index) = indices.get(&package.uuid) {
            packages[index].versions.push(version);
        } else {
            let url = Backend::Thunderstore
                .package_detail_url(game, package.uuid)
                .ok_or_else(|| eyre!("Live metadata is not supported for this game"))?;
            indices.insert(package.uuid, packages.len());
            packages.push(RequiredPackage {
                uuid: package.uuid,
                ident: package.ident.clone(),
                versions: vec![version],
                url,
            });
        }
    }
    Ok(packages)
}

async fn fetch_required_packages(
    client: &ClientWithMiddleware,
    required: &[RequiredPackage],
) -> Result<Vec<(PackageListing, DateTime<Utc>)>> {
    let mut packages = Vec::with_capacity(required.len());
    for request in required {
        let (package, observed_at) =
            thunderstore::fetch_package_detail(client, &request.url, request.uuid, &request.ident)
                .await
                .with_context(|| format!("Failed to refresh published mod {}", request.ident))?;
        for version in &request.versions {
            ensure!(
                package.get_version_with_num(version.version()).is_some(),
                "Published mod {version} is not available in Thunderstore's live listing; try again after the release becomes available"
            );
        }
        packages.push((package, observed_at));
    }
    Ok(packages)
}

/// Resolves releases that a publisher can see before the bulk catalog catches up.
/// Complete metadata validation before the importer changes profile files.
pub(super) async fn refresh_manifest_metadata(
    mods: &[R2Mod],
    game: Game,
    app: &AppHandle,
) -> Result<()> {
    let (required, cancel_token) = {
        let manager = app.lock_manager();
        ensure!(manager.active_game == game, "The active game has changed");
        let state = app.lock_thunderstore();
        (
            required_packages(mods, &state, game)?,
            state.game_context(game)?,
        )
    };
    if required.is_empty() {
        return Ok(());
    }
    let packages = tokio::select! {
        biased;
        () = cancel_token.cancelled() => bail!("The active game has changed"),
        result = fetch_required_packages(app.http(), &required) => result?,
    };

    let manager = app.lock_manager();
    let mut state = app.lock_thunderstore();
    ensure!(
        manager.active_game == game && !cancel_token.is_cancelled(),
        "The active game has changed"
    );
    state.game_context(game)?;
    // A periodic fetch may have resolved these versions while HTTP was in flight.
    let packages = packages
        .into_iter()
        .zip(&required)
        .filter_map(|(package, request)| {
            request
                .versions
                .iter()
                .any(|ident| {
                    state
                        .find_ident(ident, FromBackend::Prefer(Backend::Thunderstore))
                        .is_err()
                })
                .then_some(package)
        })
        .collect();
    thunderstore::apply_live_packages(&mut state, packages, |packages| {
        thunderstore::cache::write_live_packages(packages, game, &app.lock_prefs())
    })?;
    manager.active_profile().notify_frontend(app)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, time::Duration};

    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    use super::*;
    use crate::{game, prefs::Prefs, profile::export::R2Version, util::fs::JsonStyle};

    fn listing(name: &str, uuid: u128, version: &str, dependencies: &[&str]) -> Value {
        json!({
            "full_name": format!("Talent-{name}"),
            "uuid4": Uuid::from_u128(uuid),
            "categories": [],
            "date_created": "2024-01-01T00:00:00Z",
            "date_updated": "2024-01-01T00:00:00Z",
            "has_nsfw_content": false, "is_deprecated": false, "is_pinned": false,
            "package_url": "https://example.com/", "rating_score": 0,
            "versions": [{
                "full_name": format!("Talent-{name}-{version}"),
                "uuid4": Uuid::from_u128(uuid * 100 + u128::from(version.parse::<semver::Version>().unwrap().patch)),
                "date_created": "2024-01-01T00:00:00Z",
                "dependencies": dependencies, "description": "Profile import fixture",
                "downloads": 0, "file_size": 100, "is_active": true, "website_url": ""
            }]
        })
    }

    fn catalog(packages: &[Value]) -> (tempfile::TempDir, Prefs, Game, Thunderstore) {
        let directory = tempfile::tempdir().unwrap();
        let prefs = serde_json::from_value(json!({"dataDir": directory.path()})).unwrap();
        let game = game::from_slug("valheim").unwrap();
        fs::create_dir(directory.path().join("valheim")).unwrap();
        crate::util::fs::write_json(
            directory.path().join("valheim/thunderstore_cache.json"),
            packages,
            JsonStyle::Compact,
        )
        .unwrap();
        let mut state = Thunderstore::new();
        state
            .backend_mut(Backend::Thunderstore)
            .read_and_insert_cache(game, &prefs);
        (directory, prefs, game, state)
    }

    fn published(name: &str, version: &str) -> R2Mod {
        R2Mod {
            ident: format!("Talent-{name}").parse().unwrap(),
            version: R2Version::from(version.parse::<semver::Version>().unwrap()),
            enabled: false,
            source: Backend::Thunderstore,
        }
    }

    fn client() -> ClientWithMiddleware {
        reqwest_middleware::ClientBuilder::new(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap(),
        )
        .build()
    }

    async fn serve(body: Value, status: &str) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = body.to_string();
        let status = status.to_owned();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                assert_ne!(socket.read_buf(&mut request).await.unwrap(), 0);
            }
            socket.write_all(format!("HTTP/1.1 {status}\r\nDate: Mon, 01 Jan 2024 00:35:00 GMT\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}/live"), server)
    }

    #[tokio::test]
    async fn subscriber_resolves_the_exact_published_release_on_the_first_attempt() {
        let (_directory, prefs, game, mut state) =
            catalog(&[listing("DeepNorthCompat", 1, "1.1.3", &[])]);
        let entry = published("DeepNorthCompat", "1.1.4");
        assert!(
            entry.to_install(&state).is_err(),
            "the subscriber catalog must lag the publication"
        );
        let mut required = required_packages(std::slice::from_ref(&entry), &state, game).unwrap();
        let (url, server) = serve(listing("DeepNorthCompat", 1, "1.1.4", &[]), "200 OK").await;
        required[0].url = url;

        let packages = fetch_required_packages(&client(), &required).await.unwrap();
        thunderstore::apply_live_packages(&mut state, packages, |packages| {
            thunderstore::cache::write_live_packages(packages, game, &prefs)
        })
        .unwrap();
        let install = entry.to_install(&state).unwrap();

        assert_eq!(install.mod_id().version_uuid, Uuid::from_u128(104));
        assert!(!install.enabled());
        assert!(
            required_packages(&[entry], &state, game)
                .unwrap()
                .is_empty()
        );
        assert!(server.await.unwrap().starts_with("GET /live "));
    }

    #[test]
    fn available_versions_do_not_request_live_metadata() {
        let (_directory, _prefs, game, state) =
            catalog(&[listing("DeepNorthCompat", 1, "1.1.3", &[])]);
        assert!(
            required_packages(&[published("DeepNorthCompat", "1.1.3")], &state, game)
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_live_listing_missing_the_published_version_keeps_the_existing_cache() {
        let (directory, _prefs, game, state) =
            catalog(&[listing("DeepNorthCompat", 1, "1.1.3", &[])]);
        let path = directory.path().join("valheim/thunderstore_cache.json");
        let before = fs::read(&path).unwrap();
        let mut required =
            required_packages(&[published("DeepNorthCompat", "1.1.4")], &state, game).unwrap();
        let (url, server) = serve(listing("DeepNorthCompat", 1, "1.1.3", &[]), "200 OK").await;
        required[0].url = url;

        let error = fetch_required_packages(&client(), &required)
            .await
            .unwrap_err();

        assert!(format!("{error:#}").contains("Talent-DeepNorthCompat-1.1.4"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            state
                .find_package("Talent-DeepNorthCompat", Backend::Thunderstore)
                .unwrap()
                .latest()
                .version(),
            "1.1.3"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn live_http_failure_is_reported_with_the_package_name() {
        let (_directory, _prefs, game, state) =
            catalog(&[listing("DeepNorthCompat", 1, "1.1.3", &[])]);
        let mut required =
            required_packages(&[published("DeepNorthCompat", "1.1.4")], &state, game).unwrap();
        let (url, server) = serve(json!({}), "503 Service Unavailable").await;
        required[0].url = url;
        let error = fetch_required_packages(&client(), &required)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("Talent-DeepNorthCompat"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_release_and_its_new_dependency_are_validated_as_one_batch() {
        let (_directory, prefs, game, mut state) = catalog(&[
            listing("DeepNorthCompat", 1, "1.1.3", &["Talent-Loader-1.0.0"]),
            listing("Loader", 2, "1.0.0", &[]),
        ]);
        let entries = [
            published("DeepNorthCompat", "1.1.4"),
            published("Loader", "1.0.1"),
        ];
        let mut required = required_packages(&entries, &state, game).unwrap();
        let (compat_url, compat_server) = serve(
            listing("DeepNorthCompat", 1, "1.1.4", &["Talent-Loader-1.0.1"]),
            "200 OK",
        )
        .await;
        let mut loader = listing("Loader", 2, "1.0.1", &[]);
        loader["versions"].as_array_mut().unwrap().extend(
            listing("Loader", 2, "1.0.0", &[])["versions"]
                .as_array()
                .unwrap()
                .iter()
                .cloned(),
        );
        let (loader_url, loader_server) = serve(loader, "200 OK").await;
        required[0].url = compat_url;
        required[1].url = loader_url;
        let packages = fetch_required_packages(&client(), &required).await.unwrap();

        thunderstore::apply_live_packages(&mut state, packages, |packages| {
            thunderstore::cache::write_live_packages(packages, game, &prefs)
        })
        .unwrap();

        assert!(entries.iter().all(|entry| entry.to_install(&state).is_ok()));
        compat_server.await.unwrap();
        loader_server.await.unwrap();
    }
}
