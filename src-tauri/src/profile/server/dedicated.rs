//! Which games ship a dedicated server, and where to find it.
//!
//! This metadata lives here rather than in `games.json` because that list is
//! refreshed at runtime from upstream, which doesn't carry it.

use std::{collections::HashMap, sync::LazyLock};

use serde::Deserialize;

use crate::game::{Game, platform::Platforms};

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DedicatedServer<'a> {
    #[serde(borrow, default)]
    pub platforms: Platforms<'a>,

    pub default_port: u16,
}

/// Dedicated servers keyed by game slug.
static DEDICATED_SERVERS: LazyLock<HashMap<&'static str, DedicatedServer<'static>>> =
    LazyLock::new(|| serde_json::from_str(include_str!("dedicated_servers.json")).unwrap());

/// The dedicated server `game` ships, if it has one.
pub fn for_game(game: Game) -> Option<&'static DedicatedServer<'static>> {
    DEDICATED_SERVERS.get(&*game.slug)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game;

    #[test]
    fn every_dedicated_server_belongs_to_a_known_game() {
        for (slug, server) in DEDICATED_SERVERS.iter() {
            let game = game::from_slug(slug)
                .unwrap_or_else(|| panic!("'{slug}' is not a game in games.json"));

            assert!(
                server.platforms.iter().next().is_some(),
                "the {} dedicated server lists no platform to find it on",
                game.name
            );
        }
    }
}
