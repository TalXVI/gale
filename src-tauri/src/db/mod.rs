use std::{
    collections::HashSet,
    env, iter,
    path::PathBuf,
    sync::{Mutex, MutexGuard},
};

use eyre::{Context, Result, ensure, eyre};
use include_dir::include_dir;
use rusqlite::{OptionalExtension, params, types::Type as SqliteType};
use rusqlite_migration::{MigrationDefinitionError, Migrations};
use serde::de::DeserializeOwned;
use tracing::{info, trace};
use uuid::Uuid;

use crate::{
    prefs::Prefs,
    profile::{self, ManagedGame, ModManager, Profile, sync::auth::AuthCredentials},
    util,
};

mod migrate;

pub const FILE_NAME: &str = "data.sqlite3";
pub const SHM_FILE_NAME: &str = "data.sqlite3-shm";
pub const WAL_FILE_NAME: &str = "data.sqlite3-wal";

pub struct Db {
    conn: Mutex<rusqlite::Connection>,
    auth: keyring::Entry,
}

pub fn init() -> Result<(Db, bool)> {
    let path = util::path::default_app_data_dir().join(FILE_NAME);

    let existed = path.exists();

    info!(
        exists = existed,
        path = %path.display(),
        "opening database",
    );

    let mut conn = rusqlite::Connection::open(path).context("failed to connect")?;

    conn.pragma_update(None, "journal_mode", "WAL")
        .context("failed to set journal mode")?;

    conn.pragma_update(None, "synchronous", "normal")
        .context("failed to set synchronous mode")?;

    if env::var("GALE_DEBUG_SQL").is_ok() {
        conn.trace(Some(trace_stmt));
    }

    run_migrations(&mut conn).context("failed to run migrations")?;

    Ok((
        Db::new(
            conn,
            keyring::Entry::new("com.kesomannen.gale.sync", "session")?,
        ),
        existed,
    ))
}

fn trace_stmt(stmt: &str) {
    trace!("{stmt}");
}

static MIGRATIONS_DIR: include_dir::Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");

fn run_migrations(conn: &mut rusqlite::Connection) -> Result<()> {
    let migrations = Migrations::from_directory(&MIGRATIONS_DIR)?;

    migrations.to_latest(conn).map_err(|err| if let rusqlite_migration::Error::MigrationDefinition(
            MigrationDefinitionError::DatabaseTooFarAhead,
        ) = err { eyre!("database has been modified by a newer version of Gale, please update to the latest version") } else { eyre!(err) })?;

    Ok(())
}

fn map_json_row<I, T>(row: &rusqlite::Row, idx: I) -> rusqlite::Result<T>
where
    I: rusqlite::RowIndex,
    T: DeserializeOwned,
{
    let string = row.get::<_, String>(idx)?;
    serde_json::from_str(&string).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(0, SqliteType::Text, Box::new(err))
    })
}

fn map_json_option_row<I, T>(row: &rusqlite::Row, idx: I) -> rusqlite::Result<Option<T>>
where
    I: rusqlite::RowIndex,
    T: DeserializeOwned,
{
    match map_json_row(row, idx) {
        Ok(value) => Ok(Some(value)),
        Err(rusqlite::Error::InvalidColumnType(_, _, SqliteType::Null)) => Ok(None),
        Err(err) => Err(err),
    }
}

pub struct ManagerData {
    pub id: i64,
    pub active_game_slug: Option<String>,
    pub hidden_mods: HashSet<Uuid>,
}

pub struct ManagedGameData {
    pub id: i64,
    pub slug: String,
    pub favorite: bool,
    pub active_profile_id: i64,
}

pub struct ProfileData {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub game_slug: String,
    pub mods: Vec<profile::ProfileMod>,
    pub modpack: Option<profile::export::modpack::ModpackArgs>,
    pub ignored_updates: Option<HashSet<Uuid>>,
    pub sync_data: Option<profile::sync::SyncProfileData>,
    pub custom_args: String,
    pub ignored_package_updates: Option<HashSet<Uuid>>,
    pub excluded_export_files: Option<HashSet<PathBuf>>,
    pub server_settings: Option<profile::server::settings::ProfileServerSettings>,
}

pub struct SaveData {
    pub manager: ManagerData,
    pub games: Vec<ManagedGameData>,
    pub profiles: Vec<ProfileData>,
}

impl Db {
    pub(crate) fn new(conn: rusqlite::Connection, auth: keyring::Entry) -> Self {
        Self {
            conn: Mutex::new(conn),
            auth,
        }
    }

    fn conn(&self) -> MutexGuard<'_, rusqlite::Connection> {
        self.conn.lock().unwrap()
    }

    fn with_transaction<F>(&self, f: F) -> Result<()>
    where
        F: FnOnce(&rusqlite::Transaction) -> Result<()>,
    {
        let mut conn = self.conn();
        let tx = conn.transaction()?;

        f(&tx)?;

        tx.commit()?;
        Ok(())
    }

    pub fn next_profile_id(&self) -> Result<i64> {
        let conn = self.conn();

        let res = conn
            .prepare("SELECT MAX(id) + 1 FROM profiles")?
            .query_row((), |row| match row.get::<_, i64>(0) {
                Ok(value) => Ok(value),
                // if there are no profiles, return 1
                Err(rusqlite::Error::InvalidColumnType(_, _, SqliteType::Null)) => Ok(1),
                err => err,
            })?;

        Ok(res)
    }

    pub fn save_auth(&self, creds: Option<&AuthCredentials>) -> Result<()> {
        match creds {
            Some(creds) => self.auth.set_password(&serde_json::to_string(creds)?),
            None => match self.auth.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(error),
            },
        }
        .context("failed to persist the sync session in the OS credential store")?;

        let conn = self.conn();
        let legacy_exists: bool =
            conn.query_row("SELECT EXISTS(SELECT 1 FROM auth)", (), |row| row.get(0))?;
        if !legacy_exists {
            return Ok(());
        }
        conn.pragma_update(None, "secure_delete", true)?;
        // Arm cleanup recovery before touching the legacy store, including a
        // previous attempt that cleared the row but could not truncate its WAL.
        conn.execute(
            "INSERT OR REPLACE INTO auth (id, data) VALUES (1, NULL)",
            (),
        )?;
        Self::purge_legacy_auth(&conn)
    }

    fn purge_legacy_auth(conn: &rusqlite::Connection) -> Result<()> {
        // Rebuild old pages and truncate the WAL, including historical rotations.
        // Keep a NULL marker until cleanup succeeds so startup retries interrupted cleanup.
        conn.execute_batch("PRAGMA secure_delete=ON; UPDATE auth SET data=NULL; VACUUM;")
            .context("failed to erase legacy sync credentials from SQLite")?;
        let busy: bool = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", (), |row| row.get(0))?;
        ensure!(
            !busy,
            "legacy sync credential cleanup is blocked by another database reader; close it and retry"
        );
        conn.execute("DELETE FROM auth", ())?;
        Ok(())
    }

    pub(crate) fn read_auth(&self) -> Result<Option<AuthCredentials>> {
        let legacy: Option<Option<String>> = self
            .conn()
            .query_row("SELECT data FROM auth WHERE id=1", (), |row| row.get(0))
            .optional()?;
        let credentials = match self.auth.get_password() {
            Ok(json) => Some(
                serde_json::from_str(&json)
                    .context("invalid sync session in the OS credential store")?,
            ),
            Err(keyring::Error::NoEntry) => match legacy.as_ref().and_then(Option::as_ref) {
                Some(json) => {
                    let creds =
                        serde_json::from_str(json).context("invalid legacy sync session")?;
                    self.auth
                        .set_password(json)
                        .context("failed to migrate the sync session to the OS credential store")?;
                    Some(creds)
                }
                None => None,
            },
            Err(error) => {
                return Err(error)
                    .context("failed to read the sync session from the OS credential store");
            }
        };
        if legacy.is_some() {
            Self::purge_legacy_auth(&self.conn())?;
        }
        Ok(credentials)
    }

    pub fn read(&self) -> Result<(SaveData, Prefs, Option<AuthCredentials>, bool)> {
        if migrate::should_migrate() {
            let (data, prefs) = migrate::migrate().context("failed to migrate legacy save data")?;

            return Ok((data, prefs, None, true));
        }

        let conn = self.conn();

        let manager = conn
            .prepare("SELECT id, active_game_slug, hidden_mods FROM manager")?
            .query_row((), |row| {
                Ok(ManagerData {
                    id: row.get(0)?,
                    active_game_slug: row.get(1)?,
                    hidden_mods: map_json_row(row, 2)?,
                })
            })
            .optional()?
            .unwrap_or(ManagerData {
                id: 1,
                active_game_slug: None,
                hidden_mods: HashSet::new(),
            });

        let games = conn
            .prepare("SELECT id, slug, favorite, active_profile_id FROM managed_games")?
            .query_map((), |row| {
                Ok(ManagedGameData {
                    id: row.get(0)?,
                    slug: row.get(1)?,
                    favorite: row.get(2)?,
                    active_profile_id: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read games")?;

        let mut profiles = conn
            .prepare(
                "SELECT
                    id,
                    name,
                    path,
                    game_slug,
                    mods,
                    modpack,
                    ignored_updates,
                    sync_data,
                    custom_args,
                    ignored_package_updates,
                    excluded_export_files,
                    server_settings
                FROM profiles",
            )?
            .query_map((), |row| {
                let mut mods: Vec<profile::ProfileMod> = map_json_row(row, 4)?;
                mods.dedup_by(|a, b| a.kind.uuid() == b.kind.uuid());

                // custom args may be a json array of strings instead of one string
                let custom_args = match map_json_option_row::<_, Vec<String>>(row, 8) {
                    Ok(Some(args)) => args.join(" "),
                    Ok(None) => String::new(),
                    Err(_) => row.get(8)?,
                };

                Ok(ProfileData {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    path: row.get(2)?,
                    game_slug: row.get(3)?,
                    mods,
                    modpack: map_json_option_row(row, 5)?,
                    ignored_updates: map_json_option_row(row, 6)?,
                    sync_data: map_json_option_row(row, 7)?,
                    custom_args,
                    ignored_package_updates: map_json_option_row(row, 9)?,
                    excluded_export_files: map_json_option_row(row, 10)?,
                    server_settings: map_json_option_row(row, 11)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
            .context("failed to read profiles")?;

        profiles.sort_by(|a, b| a.name.cmp(&b.name));

        let prefs = conn
            .prepare("SELECT data FROM prefs")?
            .query_row((), |row| map_json_row(row, 0))
            .optional()
            .context("failed to read prefs")?
            .unwrap_or_default();

        drop(conn);
        let auth_state = self.read_auth()?;

        Ok((
            SaveData {
                manager,
                games,
                profiles,
            },
            prefs,
            auth_state,
            false,
        ))
    }

    pub fn delete_profile(&self, id: i64) -> Result<()> {
        self.with_transaction(|tx| {
            tx.prepare("DELETE FROM profiles WHERE id = ?")?
                .execute([id])?;

            Ok(())
        })
    }

    pub fn save_all(&self, manager: &ModManager) -> Result<()> {
        self.with_transaction(|tx| {
            self._save_manager(tx, manager)?;
            self.save_games(tx, manager.games.values())?;
            self.save_profiles(tx, manager.games.values().flat_map(|game| &game.profiles))?;

            Ok(())
        })
    }

    pub fn save_manager(&self, manager: &ModManager) -> Result<()> {
        self.with_transaction(|tx| self._save_manager(tx, manager))
    }

    fn _save_manager(&self, tx: &rusqlite::Transaction, manager: &ModManager) -> Result<()> {
        tx.execute(
            "INSERT OR REPLACE INTO manager (id, active_game_slug, hidden_mods)
            VALUES (?, ?, ?)",
            params![
                1,
                manager.active_game.slug,
                serde_json::to_string(&manager.hidden_mods)?
            ],
        )?;

        Ok(())
    }

    pub fn save_game(&self, game: &ManagedGame) -> Result<()> {
        self.with_transaction(|tx| self.save_games(tx, iter::once(game)))
    }

    fn save_games<'a>(
        &self,
        tx: &rusqlite::Transaction,
        games: impl Iterator<Item = &'a ManagedGame>,
    ) -> Result<()> {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO managed_games (id, slug, favorite, active_profile_id)
                VALUES (?, ?, ?, ?)",
        )?;

        for game in games {
            stmt.execute(params![
                game.id,
                game.game.slug,
                game.favorite,
                game.active_profile_id
            ])?;
        }

        Ok(())
    }

    pub fn save_profile(&self, profile: &Profile) -> Result<()> {
        self.with_transaction(|tx| self.save_profiles(tx, iter::once(profile)))
    }

    fn save_profiles<'a>(
        &self,
        tx: &rusqlite::Transaction,
        profiles: impl Iterator<Item = &'a Profile>,
    ) -> Result<()> {
        let mut stmt = tx.prepare(
            "INSERT OR REPLACE INTO profiles 
                (
                    id,
                    name,
                    path,
                    game_slug,
                    mods,
                    modpack,
                    ignored_updates,
                    sync_data,
                    custom_args,
                    ignored_package_updates,
                    excluded_export_files,
                    server_settings
                ) 
                VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )?;

        for profile in profiles {
            let mods = serde_json::to_string(&profile.mods)?;
            let modpack = profile
                .modpack
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let ignored_updates = serde_json::to_string(&profile.ignored_version_updates)?;
            let sync_data = profile
                .sync
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;
            let ignored_package_updates = serde_json::to_string(&profile.ignored_package_updates)?;
            let excluded_export_files = serde_json::to_string(&profile.excluded_export_files)?;
            let server_settings = profile
                .server_settings
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?;

            stmt.execute(params![
                profile.id,
                profile.name,
                profile.path.to_string_lossy(),
                profile.game.slug,
                mods,
                modpack,
                ignored_updates,
                sync_data,
                profile.custom_args,
                ignored_package_updates,
                excluded_export_files,
                server_settings,
            ])?;
        }

        Ok(())
    }

    pub fn save_prefs(&self, prefs: &Prefs) -> Result<()> {
        self.with_transaction(|tx| {
            let json = serde_json::to_string(prefs).context("failed to serialize to json")?;

            tx.prepare("INSERT OR REPLACE INTO prefs (id, data) VALUES (1, ?)")?
                .execute([json])?;

            Ok(())
        })
    }
}

#[cfg(test)]
mod auth_security_tests {
    use super::*;
    use keyring::{Entry, mock::MockCredential};

    fn credentials(refresh: &str) -> AuthCredentials {
        serde_json::from_value(serde_json::json!({"user":{"discordId":"fixture-user","name":"fixture","displayName":"Fixture","avatar":null},"accessToken":"fixture-access-secret","tokenExpiry":2000000000,"refreshToken":refresh})).unwrap()
    }

    #[test]
    fn security_keyring_only_saves_ignore_an_unrelated_sqlite_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        run_migrations(&mut conn).unwrap();
        let db = Db::new(
            conn,
            Entry::new_with_credential(Box::new(MockCredential::default())),
        );
        assert_eq!(
            db.conn()
                .query_row("SELECT COUNT(*) FROM auth", (), |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
        let reader = rusqlite::Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _: i64 = reader
            .query_row("SELECT COUNT(*) FROM profiles", (), |row| row.get(0))
            .unwrap();

        for refresh in ["initial-refresh", "rotated-refresh"] {
            db.save_auth(Some(&credentials(refresh))).unwrap();
            assert_eq!(db.read_auth().unwrap().unwrap().refresh_token(), refresh);
        }
        db.save_auth(None).unwrap();
        assert!(db.read_auth().unwrap().is_none());
        reader.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn security_migration_retries_wal_cleanup_after_a_reader_releases_its_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        run_migrations(&mut conn).unwrap();
        conn.execute(
            "INSERT INTO auth (id, data) VALUES (1, ?)",
            [serde_json::to_string(&credentials("reader-refresh-secret")).unwrap()],
        )
        .unwrap();
        let reader = rusqlite::Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let _: String = reader
            .query_row("SELECT data FROM auth", (), |row| row.get(0))
            .unwrap();
        let db = Db::new(
            conn,
            Entry::new_with_credential(Box::new(MockCredential::default())),
        );
        assert!(
            db.read_auth().is_err(),
            "migration must report that an active reader prevents erasure"
        );
        assert!(
            db.auth
                .get_password()
                .unwrap()
                .contains("reader-refresh-secret")
        );
        reader.execute_batch("ROLLBACK").unwrap();
        assert_eq!(
            db.read_auth().unwrap().unwrap().refresh_token(),
            "reader-refresh-secret"
        );
        let bytes = std::fs::read(dir.path().join(WAL_FILE_NAME)).unwrap();
        assert!(
            !bytes
                .windows("reader-refresh-secret".len())
                .any(|chunk| chunk == b"reader-refresh-secret")
        );
    }

    #[test]
    fn security_sync_session_migrates_out_of_sqlite_and_survives_storage_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        run_migrations(&mut conn).unwrap();
        let legacy = serde_json::to_string(&credentials("legacy-refresh-secret")).unwrap();
        conn.execute("INSERT INTO auth (id, data) VALUES (1, ?)", [&legacy])
            .unwrap();
        let db = Db::new(
            conn,
            Entry::new_with_credential(Box::new(MockCredential::default())),
        );
        db.auth
            .get_credential()
            .downcast_ref::<MockCredential>()
            .unwrap()
            .set_error(keyring::Error::NoStorageAccess(Box::new(
                std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            )));
        assert!(
            db.read_auth().is_err(),
            "migration must stop if the keyring cannot read the session"
        );
        let original: String = db
            .conn()
            .query_row("SELECT data FROM auth", (), |row| row.get(0))
            .unwrap();
        assert_eq!(
            original, legacy,
            "failed migration must preserve the only session"
        );
        assert_eq!(
            db.read_auth().unwrap().unwrap().refresh_token(),
            "legacy-refresh-secret"
        );
        assert!(
            db.auth
                .get_password()
                .unwrap()
                .contains("legacy-refresh-secret")
        );
        db.save_auth(Some(&credentials("replacement-refresh-secret")))
            .unwrap();
        assert_eq!(
            db.read_auth().unwrap().unwrap().refresh_token(),
            "replacement-refresh-secret"
        );
        assert!(
            db.auth
                .get_password()
                .unwrap()
                .contains("replacement-refresh-secret")
        );
        for file in [path.clone(), dir.path().join(WAL_FILE_NAME)] {
            if file.exists() {
                let bytes = std::fs::read(file).unwrap();
                for secret in [
                    "fixture-access-secret",
                    "legacy-refresh-secret",
                    "replacement-refresh-secret",
                ] {
                    assert!(
                        !bytes
                            .windows(secret.len())
                            .any(|chunk| chunk == secret.as_bytes()),
                        "SQLite retained a sync secret"
                    );
                }
            }
        }
        db.save_auth(None).unwrap();
        assert!(db.read_auth().unwrap().is_none());
        assert!(matches!(
            db.auth.get_password(),
            Err(keyring::Error::NoEntry)
        ));
    }

    #[test]
    fn security_migration_preserves_sqlite_until_the_keyring_write_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let mut conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        run_migrations(&mut conn).unwrap();
        let legacy = serde_json::to_string(&credentials("unsaved-migration-secret")).unwrap();
        conn.execute("INSERT INTO auth (id, data) VALUES (1, ?)", [&legacy])
            .unwrap();
        let store = crate::test_support::CredentialStore::default();
        store.block_writes(true);
        let db = Db::new(conn, store.entry());
        assert!(db.read_auth().is_err());
        let preserved: String = db
            .conn()
            .query_row("SELECT data FROM auth", (), |row| row.get(0))
            .unwrap();
        assert_eq!(preserved, legacy);
        assert!(matches!(
            store.entry().get_password(),
            Err(keyring::Error::NoEntry)
        ));
        store.block_writes(false);
        assert_eq!(
            db.read_auth().unwrap().unwrap().refresh_token(),
            "unsaved-migration-secret"
        );
        assert!(
            store
                .entry()
                .get_password()
                .unwrap()
                .contains("unsaved-migration-secret")
        );
        for file in [path, dir.path().join(WAL_FILE_NAME)] {
            let bytes = std::fs::read(file).unwrap();
            assert!(
                !bytes
                    .windows("unsaved-migration-secret".len())
                    .any(|bytes| bytes == b"unsaved-migration-secret")
            );
        }
    }
}
