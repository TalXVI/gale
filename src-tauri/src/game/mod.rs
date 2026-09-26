use std::{
    borrow::Cow,
    fs,
    hash::{self, Hash},
    path::Path,
    sync::LazyLock,
};

use chrono::{DateTime, Utc};
use eyre::{OptionExt, Result};
use heck::{ToKebabCase, ToPascalCase};
use serde::{Deserialize, Serialize};

use mod_loader::ModLoader;
use platform::Platforms;
use tauri::AppHandle;
use tracing::{info, warn};

use crate::{
    state::ManagerExt,
    thunderstore::Backend,
    util::{self, fs::JsonStyle},
};

pub mod mod_loader;
pub mod platform;

pub const CACHE_FILE_NAME: &str = "games.json";

const GITHUB_API_URL: &str =
    "https://api.github.com/repos/Kesomannen/gale/commits?path=src-tauri/games.json&per_page=1";
const GAMES_JSON_URL: &str =
    "https://raw.githubusercontent.com/Kesomannen/gale/refs/heads/master/src-tauri/games.json";

const BUNDLED_GAMES_JSON: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/games.json"));

const BUILD_TIME: &str = env!("BUILD_TIME");

static GAMES: LazyLock<(DateTime<Utc>, Vec<GameData<'static>>)> = LazyLock::new(|| {
    if !cfg!(debug_assertions) {
        match get_cached_games() {
            Ok(cache) => {
                info!("using cached games list, last commit at {}", cache.date);
                return (cache.date, cache.games);
            }
            Err(err) => {
                warn!("failed to read cached games list: {err}");
            }
        }
    }

    let updated_at = DateTime::parse_from_rfc3339(BUILD_TIME).unwrap().to_utc();

    info!("using bundled games list (built at {updated_at})");

    (
        updated_at,
        serde_json::from_str(BUNDLED_GAMES_JSON).unwrap(),
    )
});

#[derive(Debug, Serialize, Deserialize)]
struct GamesCache<'a> {
    date: DateTime<Utc>,
    #[serde(borrow)]
    games: Vec<GameData<'a>>,
}

fn get_cached_games() -> Result<GamesCache<'static>> {
    read_games_cache(&util::path::default_app_data_dir().join(CACHE_FILE_NAME))
}

/// Reads the downloaded `games.json` cache. Bundled dedicated-server
/// definitions are applied over the result, so a cache written by an
/// older build (or refreshed from upstream, which doesn't carry this
/// fork's metadata) can't hide dedicated-server support the binary
/// ships with.
fn read_games_cache(path: &Path) -> Result<GamesCache<'static>> {
    let str = fs::read_to_string(path)?;
    let mut cache: GamesCache<'static> = serde_json::from_str(str.leak())?;
    apply_bundled_dedicated_servers(&mut cache.games);
    Ok(cache)
}

pub async fn update_list_task(app: &AppHandle) -> Result<()> {
    if cfg!(debug_assertions) {
        info!("skipping games list update in debug mode");
        return Ok(());
    }

    let str = app
        .http()
        .get(GAMES_JSON_URL)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    let games = parse_upstream_games(&str)?;

    let date = get_last_commit_date(app).await.unwrap_or_else(|err| {
        warn!("failed to get last commit date: {err}");
        Utc::now()
    });

    let cache = GamesCache { date, games };

    let path = util::path::default_app_data_dir().join(CACHE_FILE_NAME);
    util::fs::write_json(path, &cache, JsonStyle::Pretty)?;

    info!("updated games list from github, last commit at {date}");

    Ok(())
}

/// Parses a freshly downloaded upstream `games.json`. The bundled
/// list's dedicated-server definitions are applied over it before it
/// is cached, so a refresh can't remove capabilities this build ships.
fn parse_upstream_games(json: &str) -> Result<Vec<GameData<'_>>> {
    let mut games: Vec<GameData<'_>> = serde_json::from_str(json)?;
    apply_bundled_dedicated_servers(&mut games);
    Ok(games)
}

/// The `games.json` this binary was built against.
fn bundled_games() -> &'static [GameData<'static>] {
    static BUNDLED: LazyLock<Vec<GameData<'static>>> =
        LazyLock::new(|| serde_json::from_str(BUNDLED_GAMES_JSON).unwrap());
    BUNDLED.as_slice()
}

/// Applies the bundled dedicated-server definitions over an externally
/// sourced game list — a downloaded cache or an upstream refresh.
/// Bundled metadata is authoritative for dedicated-server support:
/// upstream still supplies every other field (and can add support for
/// games the bundle doesn't define), but it cannot remove or redefine
/// a capability this binary ships with.
fn apply_bundled_dedicated_servers(games: &mut [GameData<'_>]) {
    for bundled in bundled_games() {
        let Some(dedicated_server) = &bundled.dedicated_server else {
            continue;
        };

        if let Some(game) = games.iter_mut().find(|game| game.slug == bundled.slug) {
            game.dedicated_server = Some(*dedicated_server);
        }
    }
}

async fn get_last_commit_date(app: &AppHandle) -> Result<DateTime<Utc>> {
    #[derive(Debug, Deserialize)]
    struct ResponseEntry {
        commit: Commit,
    }

    #[derive(Debug, Deserialize)]
    struct Commit {
        author: Author,
    }

    #[derive(Debug, Deserialize)]
    struct Author {
        date: DateTime<Utc>,
    }

    let response: Vec<ResponseEntry> = app
        .http()
        .get(GITHUB_API_URL)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let date = response
        .first()
        .ok_or_eyre("github api response contained no entries")?
        .commit
        .author
        .date;

    Ok(date)
}

pub type Game = &'static GameData<'static>;

pub fn list() -> impl Iterator<Item = Game> {
    GAMES.1.iter()
}

pub fn from_slug(slug: &str) -> Option<Game> {
    GAMES.1.iter().find(|game| game.slug == slug)
}

/// Games parsed strictly from the bundled `games.json`, ignoring the
/// downloaded cache. The standalone worker uses this: it may run on a
/// machine whose cache predates dedicated-server metadata (or was written
/// by an older app version), and the bundled list is the deterministic
/// source the binary was built against. The desktop uses `list()`/
/// `from_slug()`, which prefer the fresher cached list overlaid with the
/// bundled dedicated-server definitions.
#[cfg(feature = "worker")]
pub fn bundled_from_slug(slug: &str) -> Option<Game> {
    bundled_games().iter().find(|game| game.slug == slug)
}

pub fn last_updated() -> DateTime<Utc> {
    GAMES.0
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct DedicatedServer<'a> {
    #[serde(borrow, default)]
    pub platforms: Platforms<'a>,

    pub default_port: u16,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct JsonGame<'a> {
    name: &'a str,

    #[serde(default)]
    slug: Option<&'a str>,

    #[serde(default)]
    popular: bool,

    #[serde(default)]
    server: bool,

    #[serde(default, rename = "r2dirName")]
    r2_dir_name: Option<&'a str>,

    #[serde(borrow)]
    mod_loader: ModLoader<'a>,

    #[serde(borrow, default)]
    platforms: Platforms<'a>,

    #[serde(borrow, default)]
    dedicated_server: Option<DedicatedServer<'a>>,

    #[serde(default)]
    backends: Option<Vec<Backend>>,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase", from = "JsonGame")]
pub struct GameData<'a> {
    pub name: &'a str,
    pub slug: Cow<'a, str>,
    pub r2_dir_name: Cow<'a, str>,
    pub popular: bool,
    pub server: bool,
    pub mod_loader: ModLoader<'a>,
    pub platforms: Platforms<'a>,
    pub dedicated_server: Option<DedicatedServer<'a>>,
    pub backends: Vec<Backend>,
}

impl<'a> From<JsonGame<'a>> for GameData<'a> {
    fn from(value: JsonGame<'a>) -> Self {
        let JsonGame {
            name,
            slug,
            popular,
            server,
            r2_dir_name,
            mod_loader,
            platforms,
            dedicated_server,
            backends,
        } = value;

        let slug = match slug {
            Some(slug) => Cow::Borrowed(slug),
            None => Cow::Owned(name.to_kebab_case()),
        };

        let r2_dir_name = match r2_dir_name {
            Some(name) => Cow::Borrowed(name),
            None => Cow::Owned(slug.to_pascal_case()),
        };

        Self {
            name,
            slug,
            r2_dir_name,
            popular,
            server,
            mod_loader,
            platforms,
            dedicated_server,
            backends: backends.unwrap_or(vec![Backend::Thunderstore]),
        }
    }
}

impl PartialEq for GameData<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.slug == other.slug
    }
}

impl Eq for GameData<'_> {}

impl Hash for GameData<'_> {
    fn hash<H: hash::Hasher>(&self, state: &mut H) {
        self.slug.hash(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `games.json` cache written by a build that predates this
    /// fork's dedicated-server metadata: a legacy `server` flag and no
    /// `dedicatedServer` object.
    const LEGACY_CACHE: &str = r#"{
        "date": "2024-01-01T00:00:00Z",
        "games": [
            {
                "name": "Valheim",
                "slug": "valheim",
                "server": true,
                "modLoader": { "name": "BepInEx" }
            },
            {
                "name": "H3VR",
                "slug": "h3vr",
                "modLoader": { "name": "BepInEx" }
            }
        ]
    }"#;

    /// What an upstream refresh looks like to this fork:
    /// `Kesomannen/gale` carries no `dedicatedServer` on Valheim, but
    /// may add it for other games.
    const UPSTREAM_GAMES: &str = r#"[
        {
            "name": "Valheim",
            "slug": "valheim",
            "server": true,
            "popular": false,
            "modLoader": { "name": "BepInEx" }
        },
        {
            "name": "Upstream Server",
            "slug": "upstream-server",
            "modLoader": { "name": "BepInEx" },
            "dedicatedServer": {
                "platforms": { "steam": { "id": 42 } },
                "defaultPort": 1234
            }
        },
        {
            "name": "H3VR",
            "slug": "h3vr",
            "modLoader": { "name": "BepInEx" }
        }
    ]"#;

    fn bundled_dedicated_server(slug: &str) -> DedicatedServer<'static> {
        bundled_games()
            .iter()
            .find(|game| game.slug == slug)
            .and_then(|game| game.dedicated_server)
            .expect("bundled dedicated-server metadata missing")
    }

    fn find<'a>(games: &'a [GameData<'a>], slug: &str) -> &'a GameData<'a> {
        games.iter().find(|game| game.slug == slug).unwrap()
    }

    /// Loading a legacy cache restores the bundled dedicated-server
    /// metadata. Goes through `read_games_cache` — the same function
    /// `get_cached_games` calls for the release startup path — pointed
    /// at a temp dir so no test touches the real app-data cache.
    #[test]
    fn legacy_cache_load_recovers_dedicated_server() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(CACHE_FILE_NAME);
        fs::write(&path, LEGACY_CACHE).unwrap();

        let cache = read_games_cache(&path).unwrap();
        let valheim = find(&cache.games, "valheim");

        assert_eq!(
            serde_json::to_value(valheim.dedicated_server.unwrap()).unwrap(),
            serde_json::to_value(bundled_dedicated_server("valheim")).unwrap()
        );
        assert!(find(&cache.games, "h3vr").dedicated_server.is_none());
    }

    #[test]
    fn upstream_refresh_cannot_remove_dedicated_server() {
        let games = parse_upstream_games(UPSTREAM_GAMES).unwrap();
        let valheim = find(&games, "valheim");

        assert_eq!(
            serde_json::to_value(valheim.dedicated_server.unwrap()).unwrap(),
            serde_json::to_value(bundled_dedicated_server("valheim")).unwrap()
        );
        // Every other field still comes from upstream, even when it
        // disagrees with the bundle (`popular` is true upstream-side).
        assert!(!valheim.popular);
        assert_eq!(
            find(&games, "upstream-server")
                .dedicated_server
                .unwrap()
                .default_port,
            1234
        );
        assert!(find(&games, "h3vr").dedicated_server.is_none());
    }

    #[test]
    fn bundled_dedicated_server_is_authoritative() {
        let games = parse_upstream_games(
            r#"[{
                "name": "Valheim",
                "slug": "valheim",
                "modLoader": { "name": "BepInEx" },
                "dedicatedServer": {
                    "platforms": { "epicGames": { "identifier": "upstream" } },
                    "defaultPort": 1
                }
            }]"#,
        )
        .unwrap();

        assert_eq!(
            serde_json::to_value(games[0].dedicated_server.unwrap()).unwrap(),
            serde_json::to_value(bundled_dedicated_server("valheim")).unwrap()
        );
    }
}
