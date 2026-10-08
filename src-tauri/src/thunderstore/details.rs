use chrono::{DateTime, Utc};
use eyre::{Context, Result, bail, ensure, eyre};
use reqwest_middleware::ClientWithMiddleware;
use tauri::AppHandle;
use uuid::Uuid;

use super::{Backend, FromBackend, PackageIdent, PackageListing, Thunderstore};
use crate::state::ManagerExt;

/// Pulls one live listing without changing installed files or periodic fetching.
pub(super) async fn pull_live_package(
    uuid: Uuid,
    game_slug: &str,
    app: &AppHandle,
) -> Result<String> {
    let (game, url, ident, cancel_token) = {
        let manager = app.lock_manager();
        let game = manager.active_game;
        ensure!(game.slug == game_slug, "The active game has changed");
        let state = app.lock_thunderstore();
        ensure!(state.game == Some(game), "The active game has changed");
        let ident = state
            .get_package(uuid, Backend::Thunderstore)?
            .ident
            .clone();
        let url = Backend::Thunderstore
            .package_detail_url(game, uuid)
            .ok_or_else(|| eyre!("Live pulls are not supported for this game"))?;
        (game, url, ident, state.fetch_cancel_token.clone())
    };

    let (package, observed_at) = tokio::select! {
        biased;
        () = cancel_token.cancelled() => bail!("The active game has changed"),
        result = fetch_package_detail(app.http(), &url, uuid, &ident) => result?,
    };

    let manager = app.lock_manager();
    let mut state = app.lock_thunderstore();
    ensure!(
        manager.active_game == game && state.game == Some(game) && !cancel_token.is_cancelled(),
        "The active game has changed"
    );
    let version = package.latest().version().to_string();
    apply_live_packages(&mut state, vec![(package, observed_at)], |packages| {
        super::cache::write_live_packages(packages, game, &app.lock_prefs())
    })?;
    manager.active_profile().notify_frontend(app)?;
    Ok(version)
}

pub(crate) fn apply_live_packages(
    state: &mut Thunderstore,
    packages: Vec<(PackageListing, DateTime<Utc>)>,
    persist: impl FnOnce(&[(PackageListing, DateTime<Utc>)]) -> Result<()>,
) -> Result<()> {
    packages
        .iter()
        .try_for_each(|(package, _)| validate_live_package(state, package, &packages))?;
    persist(&packages)?;
    for (package, observed_at) in packages {
        state
            .backend_mut(package.backend)
            .insert_live_package(package, observed_at);
    }
    Ok(())
}

fn validate_live_package(
    state: &Thunderstore,
    package: &PackageListing,
    pending: &[(PackageListing, DateTime<Utc>)],
) -> Result<()> {
    let current = state.get_package(package.uuid, Backend::Thunderstore)?;
    ensure!(
        package.date_updated >= current.date_updated,
        "The live API still returns an older release snapshot; try again shortly"
    );
    let latest = package.latest();
    let released = package.latest_released();
    // Historical versions already exposed by the catalog need not block a live pull.
    let dependencies = package
        .versions
        .iter()
        .filter(|version| {
            version.uuid == latest.uuid
                || version.uuid == released.uuid
                || current
                    .get_version(version.uuid)
                    .is_none_or(|old| old.dependencies != version.dependencies)
        })
        .flat_map(|version| {
            version
                .dependencies
                .iter()
                .map(|ident| (ident, package.backend))
        });

    super::ensure_dependencies_available(dependencies, |ident, backend| {
        if let Some((pending_package, _)) = pending
            .iter()
            .find(|(p, _)| p.backend == backend && ident.full_name() == p.full_name())
        {
            let version = pending_package
                .get_version_with_num(ident.version())
                .ok_or_else(|| eyre!("Required version {ident} is not in the live listing"))?;
            Ok((pending_package, version).into())
        } else {
            state.find_ident(ident, FromBackend::Prefer(backend))
        }
    })
    .context(
        "Cannot pull live metadata yet; fetch mods again after Thunderstore's catalog updates",
    )?;

    Ok(())
}

pub(crate) async fn fetch_package_detail(
    client: &ClientWithMiddleware,
    url: &str,
    expected_uuid: Uuid,
    expected_ident: &PackageIdent,
) -> Result<(PackageListing, DateTime<Utc>)> {
    let response = client.get(url).send().await?.error_for_status()?;
    let observed_at = DateTime::parse_from_rfc2822(
        response
            .headers()
            .get(reqwest::header::DATE)
            .ok_or_else(|| eyre!("The live API response has no freshness timestamp"))?
            .to_str()?,
    )?
    .to_utc();
    let package: PackageListing = response.json().await?;

    ensure!(
        package.uuid == expected_uuid,
        "package listing has an unexpected UUID"
    );
    ensure!(
        !package.versions.is_empty(),
        "package listing has no active versions"
    );
    ensure!(
        package.ident == *expected_ident,
        "package listing has an unexpected name"
    );
    for version in &package.versions {
        ensure!(
            version.is_active && version.ident.full_name() == package.full_name(),
            "package listing contains an invalid version"
        );
        version
            .version()
            .parse::<semver::Version>()
            .context("package listing contains an invalid version number")?;
    }

    Ok((package, observed_at))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        task::JoinHandle,
    };

    use super::*;
    use crate::{
        profile::install::ModInstall,
        thunderstore::{BorrowedMod, ModId, backend::ThunderstoreBackend},
    };

    fn apply_live_package(
        state: &mut Thunderstore,
        package: PackageListing,
        observed_at: DateTime<Utc>,
    ) -> Result<String> {
        let version = package.latest().version().to_string();
        apply_live_packages(state, vec![(package, observed_at)], |_| Ok(()))?;
        Ok(version)
    }

    fn listing(versions: &[&str], updated_at: &str) -> Value {
        json!({
            "full_name": "Example-Compat",
            "categories": ["Mods"],
            "date_created": "2024-01-01T00:00:00Z",
            "date_updated": updated_at,
            "has_nsfw_content": false,
            "is_deprecated": false,
            "is_pinned": false,
            "package_url": "https://thunderstore.io/c/valheim/p/Example/Compat/",
            "rating_score": 0,
            "uuid4": Uuid::from_u128(1),
            "versions": versions.iter().map(|version| {
                let parsed: semver::Version = version.parse().unwrap();
                json!({
                    "full_name": format!("Example-Compat-{version}"),
                    "date_created": "2024-01-01T00:00:00Z",
                    "dependencies": ["Example-Loader-1.0.0"],
                    "description": "Compatibility fixes",
                    "downloads": 0,
                    "file_size": 159104,
                    "is_active": true,
                    "uuid4": Uuid::from_u128(10 + u128::from(parsed.patch)),
                    "website_url": "https://example.com/"
                })
            }).collect::<Vec<_>>()
        })
    }

    fn package(versions: &[&str], updated_at: &str) -> PackageListing {
        serde_json::from_value(listing(versions, updated_at)).unwrap()
    }

    fn loader(dependencies: &[&str]) -> PackageListing {
        let mut value = listing(&["1.0.0"], "2024-01-01T00:00:00Z");
        value["full_name"] = json!("Example-Loader");
        value["uuid4"] = json!(Uuid::from_u128(2));
        value["versions"][0]["full_name"] = json!("Example-Loader-1.0.0");
        value["versions"][0]["dependencies"] = json!(dependencies);
        serde_json::from_value(value).unwrap()
    }

    fn observed_at() -> DateTime<Utc> {
        "2024-01-01T00:35:00Z".parse().unwrap()
    }

    fn stale_snapshot() -> Option<DateTime<Utc>> {
        Some("2024-01-01T00:34:30Z".parse().unwrap())
    }

    fn catalog() -> Thunderstore {
        let mut state = Thunderstore::new();
        let backend = state.backend_mut(Backend::Thunderstore);
        backend.insert_package(package(&["1.1.3"], "2024-01-01T00:20:00Z"), None);
        backend.insert_package(loader(&[]), None);
        state
    }

    async fn serve_once(status: &str, body: String) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                assert_ne!(socket.read_buf(&mut request).await.unwrap(), 0);
            }
            let response = format!(
                "HTTP/1.1 {status}\r\nDate: Mon, 01 Jan 2024 00:35:00 GMT\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (
            format!("http://{address}/package/{}", Uuid::from_u128(1)),
            server,
        )
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

    #[tokio::test]
    async fn live_pull_exposes_a_resolvable_update_before_the_bulk_feed_catches_up() {
        let uuid = Uuid::from_u128(1);
        let mut state = catalog();
        let stale = package(&["1.1.3"], "2024-01-01T00:20:00Z");

        let (url, server) = serve_once(
            "200 OK",
            listing(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z").to_string(),
        )
        .await;
        let (fresh, observed_at) = fetch_package_detail(&client(), &url, uuid, &stale.ident)
            .await
            .unwrap();
        apply_live_package(&mut state, fresh, observed_at).unwrap();
        state.backend_mut(Backend::Thunderstore).replace_packages(
            [(uuid, stale), (Uuid::from_u128(2), loader(&[]))]
                .into_iter()
                .collect(),
            stale_snapshot(),
        );

        let latest = BorrowedMod::latest(state.get_package(uuid, Backend::Thunderstore).unwrap());
        let id: ModId = latest.into();
        let install = serde_json::to_value(ModInstall::new(latest)).unwrap();
        assert_eq!(
            (
                latest.version.version(),
                id.version_uuid,
                install["fileSize"].as_u64()
            ),
            ("1.1.4", Uuid::from_u128(14), Some(159104))
        );
        assert_eq!(
            state
                .get_mod(uuid, Uuid::from_u128(13), Backend::Thunderstore)
                .unwrap()
                .version
                .version(),
            "1.1.3"
        );
        state
            .ensure_dependencies_available(latest.dependencies())
            .unwrap();
        assert_eq!(state.dependencies(latest.dependencies()).count(), 1);
        assert!(
            server
                .await
                .unwrap()
                .starts_with(&format!("GET /package/{uuid} HTTP/1.1\r\n"))
        );
    }

    #[test]
    fn streaming_bulk_fetch_preserves_an_explicit_live_pull() {
        let uuid = Uuid::from_u128(1);
        let mut backend = ThunderstoreBackend::new(Backend::Thunderstore);
        backend.insert_live_package(
            package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z"),
            observed_at(),
        );
        backend.insert_package(
            package(&["1.1.3"], "2024-01-01T00:20:00Z"),
            stale_snapshot(),
        );

        assert_eq!(
            backend.get_package(uuid).unwrap().latest().version(),
            "1.1.4"
        );
    }

    #[test]
    fn live_withdrawal_survives_a_stale_catalog_with_the_same_publication_timestamp() {
        let uuid = Uuid::from_u128(1);
        let mut backend = ThunderstoreBackend::new(Backend::Thunderstore);
        backend.insert_live_package(package(&["1.1.3"], "2024-01-01T00:34:00Z"), observed_at());
        backend.replace_packages(
            [(uuid, package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z"))]
                .into_iter()
                .collect(),
            stale_snapshot(),
        );

        assert_eq!(
            backend.get_package(uuid).unwrap().latest().version(),
            "1.1.3"
        );
    }

    #[test]
    fn replacement_catalog_removes_unlisted_packages() {
        let mut backend = ThunderstoreBackend::new(Backend::Thunderstore);
        backend.insert_live_package(package(&["1.1.3"], "2024-01-01T00:20:00Z"), observed_at());
        backend.replace_packages(
            Default::default(),
            Some("2024-01-01T00:40:00Z".parse().unwrap()),
        );

        assert!(backend.get_package(Uuid::from_u128(1)).is_err());
    }

    #[tokio::test]
    async fn wrong_package_uuid_is_reported() {
        let (url, server) = serve_once(
            "200 OK",
            listing(&["1.1.4"], "2024-01-01T00:34:00Z").to_string(),
        )
        .await;
        let error = fetch_package_detail(
            &client(),
            &url,
            Uuid::from_u128(2),
            &package(&["1.1.3"], "2024-01-01T00:20:00Z").ident,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("unexpected UUID"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn empty_version_list_is_reported() {
        let (url, server) =
            serve_once("200 OK", listing(&[], "2024-01-01T00:34:00Z").to_string()).await;
        let error = fetch_package_detail(
            &client(),
            &url,
            Uuid::from_u128(1),
            &package(&["1.1.3"], "2024-01-01T00:20:00Z").ident,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("no active versions"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rate_limit_remains_identifiable_after_adding_package_context() {
        let (url, server) = serve_once("429 Too Many Requests", "{}".into()).await;
        let error = fetch_package_detail(
            &client(),
            &url,
            Uuid::from_u128(1),
            &package(&["1.1.3"], "2024-01-01T00:20:00Z").ident,
        )
        .await
        .unwrap_err()
        .wrap_err("failed to refresh installed package");

        assert_eq!(
            error.downcast_ref::<reqwest::Error>().unwrap().status(),
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS)
        );
        server.await.unwrap();
    }

    #[test]
    fn missing_dependency_version_rejects_the_pull_without_exposing_the_update() {
        let mut state = catalog();
        let mut fresh = package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z");
        fresh.versions[0].dependencies = vec!["Example-Loader-2.0.0".parse().unwrap()];

        let error = apply_live_package(&mut state, fresh, observed_at()).unwrap_err();

        assert!(format!("{error:#}").contains("Example-Loader-2.0.0"));
        assert_eq!(
            state
                .get_package(Uuid::from_u128(1), Backend::Thunderstore)
                .unwrap()
                .latest()
                .version(),
            "1.1.3"
        );
    }

    #[test]
    fn installation_validation_reports_missing_transitive_dependencies() {
        let mut state = catalog();
        state
            .backend_mut(Backend::Thunderstore)
            .insert_package(loader(&["Example-Missing-1.0.0"]), None);
        let root = BorrowedMod::latest(
            state
                .get_package(Uuid::from_u128(1), Backend::Thunderstore)
                .unwrap(),
        );

        let error = state
            .ensure_dependencies_available(root.dependencies())
            .unwrap_err();

        assert!(format!("{error:#}").contains("Example-Missing-1.0.0"));
    }

    #[test]
    fn dependency_cycles_resolve_without_looping() {
        let mut state = catalog();
        state
            .backend_mut(Backend::Thunderstore)
            .insert_package(loader(&["Example-Compat-1.1.3"]), None);
        let root = BorrowedMod::latest(
            state
                .get_package(Uuid::from_u128(1), Backend::Thunderstore)
                .unwrap(),
        );

        state
            .ensure_dependencies_available(root.dependencies())
            .unwrap();
    }

    #[test]
    fn retired_historical_dependencies_do_not_block_a_new_resolvable_release() {
        let mut state = catalog();
        let mut old = package(&["1.1.3", "1.1.2"], "2024-01-01T00:20:00Z");
        old.versions[1].dependencies = vec!["Example-Retired-1.0.0".parse().unwrap()];
        state
            .backend_mut(Backend::Thunderstore)
            .insert_package(old.clone(), None);
        let mut fresh = package(&["1.1.4", "1.1.3", "1.1.2"], "2024-01-01T00:34:00Z");
        fresh.versions[2].dependencies = old.versions[1].dependencies.clone();

        assert_eq!(
            apply_live_package(&mut state, fresh, observed_at()).unwrap(),
            "1.1.4"
        );
    }

    #[test]
    fn a_newer_catalog_release_replaces_a_live_pull() {
        let uuid = Uuid::from_u128(1);
        let mut backend = ThunderstoreBackend::new(Backend::Thunderstore);
        backend.insert_live_package(
            package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z"),
            observed_at(),
        );
        backend.replace_packages(
            [(
                uuid,
                package(&["1.1.5", "1.1.4", "1.1.3"], "2024-01-01T00:40:00Z"),
            )]
            .into_iter()
            .collect(),
            Some("2024-01-01T00:40:00Z".parse().unwrap()),
        );

        assert_eq!(
            backend.get_package(uuid).unwrap().latest().version(),
            "1.1.5"
        );
    }

    #[test]
    fn a_stale_catalog_cannot_remove_a_live_listing_it_does_not_contain_yet() {
        let uuid = Uuid::from_u128(1);
        let mut backend = ThunderstoreBackend::new(Backend::Thunderstore);
        backend.insert_live_package(package(&["1.1.4"], "2024-01-01T00:34:00Z"), observed_at());
        backend.replace_packages(Default::default(), stale_snapshot());

        assert_eq!(
            backend.get_package(uuid).unwrap().latest().version(),
            "1.1.4"
        );
    }

    #[test]
    fn a_later_catalog_releases_the_live_override_for_further_metadata_changes() {
        let uuid = Uuid::from_u128(1);
        let mut backend = ThunderstoreBackend::new(Backend::Thunderstore);
        let fresh = package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z");
        backend.insert_live_package(fresh.clone(), observed_at());
        backend.insert_package(fresh, Some("2024-01-01T00:40:00Z".parse().unwrap()));
        let mut deprecated = package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z");
        deprecated.is_deprecated = true;
        backend.insert_package(deprecated, stale_snapshot());

        assert!(backend.get_package(uuid).unwrap().is_deprecated);
    }

    #[test]
    fn later_catalog_withdrawal_with_unchanged_publication_date_is_applied() {
        let uuid = Uuid::from_u128(1);
        let mut backend = ThunderstoreBackend::new(Backend::Thunderstore);
        backend.insert_live_package(
            package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z"),
            observed_at(),
        );
        backend.replace_packages(
            [(uuid, package(&["1.1.3"], "2024-01-01T00:34:00Z"))]
                .into_iter()
                .collect(),
            Some("2024-01-01T00:40:00Z".parse().unwrap()),
        );

        assert_eq!(
            backend.get_package(uuid).unwrap().latest().version(),
            "1.1.3"
        );
    }

    #[test]
    fn an_older_live_api_response_cannot_downgrade_a_newer_catalog_release() {
        let mut state = catalog();
        let uuid = Uuid::from_u128(1);
        state
            .backend_mut(Backend::Thunderstore)
            .insert_package(package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z"), None);

        let error = apply_live_package(
            &mut state,
            package(&["1.1.3"], "2024-01-01T00:20:00Z"),
            observed_at(),
        )
        .unwrap_err();

        assert!(error.to_string().contains("older release snapshot"));
        assert_eq!(
            state
                .get_package(uuid, Backend::Thunderstore)
                .unwrap()
                .latest()
                .version(),
            "1.1.4"
        );
    }

    #[test]
    fn a_catalog_uploaded_later_cannot_undo_a_newer_publication() {
        let uuid = Uuid::from_u128(1);
        let mut backend = ThunderstoreBackend::new(Backend::Thunderstore);
        backend.insert_live_package(
            package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z"),
            observed_at(),
        );
        backend.insert_package(
            package(&["1.1.3"], "2024-01-01T00:20:00Z"),
            Some("2024-01-01T00:40:00Z".parse().unwrap()),
        );

        assert_eq!(
            backend.get_package(uuid).unwrap().latest().version(),
            "1.1.4"
        );
    }

    #[test]
    fn installed_live_version_survives_cache_reload_and_a_stale_startup_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let prefs: crate::prefs::Prefs =
            serde_json::from_value(json!({ "dataDir": dir.path() })).unwrap();
        let game = crate::game::from_slug("valheim").unwrap();
        std::fs::create_dir(dir.path().join("valheim")).unwrap();
        let mut state = catalog();
        apply_live_package(
            &mut state,
            package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z"),
            observed_at(),
        )
        .unwrap();
        let installed: ModId = BorrowedMod::latest(
            state
                .get_package(Uuid::from_u128(1), Backend::Thunderstore)
                .unwrap(),
        )
        .into();
        super::super::cache::write_packages(
            state
                .backend(Backend::Thunderstore)
                .packages
                .values()
                .collect(),
            game,
            &prefs,
            &state,
        )
        .unwrap();
        let backend = state.backend_mut(Backend::Thunderstore);
        backend.clear_packages();
        backend.read_and_insert_cache(game, &prefs);
        backend.insert_package(
            package(&["1.1.3"], "2024-01-01T00:20:00Z"),
            stale_snapshot(),
        );

        assert_eq!(installed.borrow(&state).unwrap().version.version(), "1.1.4");
    }

    #[test]
    fn live_metadata_freshness_survives_cache_reload_with_unchanged_publication_date() {
        let dir = tempfile::tempdir().unwrap();
        let prefs: crate::prefs::Prefs =
            serde_json::from_value(json!({ "dataDir": dir.path() })).unwrap();
        let game = crate::game::from_slug("valheim").unwrap();
        std::fs::create_dir(dir.path().join("valheim")).unwrap();
        let mut state = catalog();
        let mut deprecated = package(&["1.1.3"], "2024-01-01T00:20:00Z");
        deprecated.is_deprecated = true;
        apply_live_package(&mut state, deprecated, observed_at()).unwrap();
        super::super::cache::write_packages(
            state
                .backend(Backend::Thunderstore)
                .packages
                .values()
                .collect(),
            game,
            &prefs,
            &state,
        )
        .unwrap();
        let backend = state.backend_mut(Backend::Thunderstore);
        backend.clear_packages();
        backend.read_and_insert_cache(game, &prefs);
        backend.insert_package(
            package(&["1.1.3"], "2024-01-01T00:20:00Z"),
            stale_snapshot(),
        );

        assert!(
            backend
                .get_package(Uuid::from_u128(1))
                .unwrap()
                .is_deprecated
        );
    }

    #[test]
    fn a_live_pull_is_persisted_before_any_install_and_retained_by_later_cache_writes() {
        let dir = tempfile::tempdir().unwrap();
        let prefs: crate::prefs::Prefs =
            serde_json::from_value(json!({ "dataDir": dir.path() })).unwrap();
        let game = crate::game::from_slug("valheim").unwrap();
        std::fs::create_dir(dir.path().join("valheim")).unwrap();
        let mut state = catalog();
        apply_live_packages(
            &mut state,
            vec![(
                package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z"),
                observed_at(),
            )],
            |packages| super::super::cache::write_live_packages(packages, game, &prefs),
        )
        .unwrap();
        let mut reloaded = ThunderstoreBackend::new(Backend::Thunderstore);
        reloaded.read_and_insert_cache(game, &prefs);
        assert_eq!(
            reloaded
                .get_package(Uuid::from_u128(1))
                .unwrap()
                .latest()
                .version(),
            "1.1.4"
        );

        let other = state
            .get_package(Uuid::from_u128(2), Backend::Thunderstore)
            .unwrap();
        super::super::cache::write_packages(vec![other], game, &prefs, &state).unwrap();
        reloaded.clear_packages();
        reloaded.read_and_insert_cache(game, &prefs);
        reloaded.insert_package(
            package(&["1.1.3"], "2024-01-01T00:20:00Z"),
            stale_snapshot(),
        );
        assert_eq!(
            reloaded
                .get_package(Uuid::from_u128(1))
                .unwrap()
                .latest()
                .version(),
            "1.1.4"
        );
    }

    #[test]
    fn persisted_live_cache_remains_readable_as_a_legacy_listing_array() {
        let dir = tempfile::tempdir().unwrap();
        let prefs: crate::prefs::Prefs =
            serde_json::from_value(json!({ "dataDir": dir.path() })).unwrap();
        let game = crate::game::from_slug("valheim").unwrap();
        std::fs::create_dir(dir.path().join("valheim")).unwrap();
        let fresh = package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z");
        super::super::cache::write_live_packages(&[(fresh.clone(), observed_at())], game, &prefs)
            .unwrap();
        let old_format: Vec<PackageListing> =
            crate::util::fs::read_json(dir.path().join("valheim/thunderstore_cache.json")).unwrap();
        assert_eq!(old_format, [fresh]);
    }

    #[test]
    fn failed_live_cache_write_keeps_the_previous_metadata_and_cache_contents() {
        let dir = tempfile::tempdir().unwrap();
        let prefs: crate::prefs::Prefs =
            serde_json::from_value(json!({ "dataDir": dir.path() })).unwrap();
        let game = crate::game::from_slug("valheim").unwrap();
        std::fs::create_dir(dir.path().join("valheim")).unwrap();
        let path = dir.path().join("valheim/thunderstore_cache.json");
        std::fs::write(&path, b"malformed existing cache").unwrap();
        let mut state = catalog();
        let error = apply_live_packages(
            &mut state,
            vec![(
                package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z"),
                observed_at(),
            )],
            |packages| super::super::cache::write_live_packages(packages, game, &prefs),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("failed to deserialize cache"));
        assert_eq!(std::fs::read(&path).unwrap(), b"malformed existing cache");
        assert_eq!(
            state
                .get_package(Uuid::from_u128(1), Backend::Thunderstore)
                .unwrap()
                .latest()
                .version(),
            "1.1.3"
        );
    }

    #[test]
    fn restored_live_metadata_allows_a_fresh_catalog_withdrawal() {
        let dir = tempfile::tempdir().unwrap();
        let prefs: crate::prefs::Prefs =
            serde_json::from_value(json!({ "dataDir": dir.path() })).unwrap();
        let game = crate::game::from_slug("valheim").unwrap();
        std::fs::create_dir(dir.path().join("valheim")).unwrap();
        super::super::cache::write_live_packages(
            &[(
                package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z"),
                observed_at(),
            )],
            game,
            &prefs,
        )
        .unwrap();
        let mut backend = ThunderstoreBackend::new(Backend::Thunderstore);
        backend.read_and_insert_cache(game, &prefs);
        backend.insert_package(
            package(&["1.1.3"], "2024-01-01T00:34:00Z"),
            Some("2024-01-01T00:40:00Z".parse().unwrap()),
        );
        assert_eq!(
            backend
                .get_package(Uuid::from_u128(1))
                .unwrap()
                .latest()
                .version(),
            "1.1.3"
        );
        assert!(backend.live_observed_at(Uuid::from_u128(1)).is_none());
    }

    #[test]
    fn an_unresolvable_dependency_rejects_the_entire_live_batch_before_persistence() {
        let mut state = catalog();
        let fresh = package(&["1.1.4", "1.1.3"], "2024-01-01T00:34:00Z");
        let invalid_loader = loader(&["Example-Missing-1.0.0"]);
        let mut persisted = false;
        let error = apply_live_packages(
            &mut state,
            vec![(fresh, observed_at()), (invalid_loader, observed_at())],
            |_| {
                persisted = true;
                Ok(())
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("Example-Missing-1.0.0"));
        assert!(!persisted);
        assert_eq!(
            state
                .get_package(Uuid::from_u128(1), Backend::Thunderstore)
                .unwrap()
                .latest()
                .version(),
            "1.1.3"
        );
    }
}
