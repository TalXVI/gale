use std::{
    borrow::Cow,
    hash::{self, Hash},
    sync::LazyLock,
};

use chrono::{DateTime, Utc};
use heck::{ToKebabCase, ToPascalCase};
use serde::{Deserialize, Serialize};

use crate::thunderstore::Backend;
use mod_loader::ModLoader;
use platform::Platforms;

pub mod mod_loader;
pub mod platform;

const BUNDLED_GAMES_JSON: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/games.json"));
const BUILD_TIME: &str = env!("BUILD_TIME");

// Installation rules are executable policy and travel with the signed application.
// Never load the legacy unsigned games.json cache.
static GAMES: LazyLock<(DateTime<Utc>, Vec<GameData<'static>>)> = LazyLock::new(|| {
    let updated_at = DateTime::parse_from_rfc3339(BUILD_TIME).unwrap().to_utc();
    (
        updated_at,
        serde_json::from_str(BUNDLED_GAMES_JSON).unwrap(),
    )
});

pub type Game = &'static GameData<'static>;

pub fn list() -> impl Iterator<Item = Game> {
    GAMES.1.iter()
}

pub fn from_slug(slug: &str) -> Option<Game> {
    GAMES.1.iter().find(|game| game.slug == slug)
}

pub fn last_updated() -> DateTime<Utc> {
    GAMES.0
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
