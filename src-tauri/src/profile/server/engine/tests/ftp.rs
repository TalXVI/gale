use super::*;

// ---------- end-to-end over the in-memory FTP server ----------
//
// These run the real FTP transport against `remote::fake_ftp`,
// so the transport's command choices (TYPE I, MLST, RETR) are part
// of what is verified.

fn ftp_ops(addr: std::net::SocketAddr) -> Result<Box<dyn RemoteOps>> {
    let settings = TransportSettings {
        protocol: RemoteProtocol::Ftp,
        host: "127.0.0.1".to_owned(),
        port: addr.port(),
        username: "u".to_owned(),
        server_directory: "/".to_owned(),
        authentication: RemoteAuthentication::Password,
        ..Default::default()
    };
    match remote::connect(&settings, "pw")? {
        ConnectionAttempt::Connected(conn) => Ok(conn),
        _ => {
            eyre::bail!("unexpected trust prompt from the fake FTP server");
        }
    }
}

fn open_ftp(addr: std::net::SocketAddr) -> Result<Session> {
    let spec = spec();
    open_session(ftp_ops(addr)?, &spec, RemotePathBuf::new("/").unwrap())
}

/// Same connection path as `ftp_ops`, but over explicit FTPS pinned
/// to the fake's self-signed certificate.
fn ftps_connection(server: &remote::fake_ftp::FakeFtp) -> Result<Box<dyn RemoteOps>> {
    let settings = TransportSettings {
        protocol: RemoteProtocol::Ftps,
        host: "127.0.0.1".to_owned(),
        port: server.addr.port(),
        username: "u".to_owned(),
        server_directory: "/".to_owned(),
        authentication: RemoteAuthentication::Password,
        trusted_certificate: server.trusted_certificate(),
        ..Default::default()
    };
    match remote::connect(&settings, "pw")? {
        ConnectionAttempt::Connected(conn) => Ok(conn),
        _ => {
            eyre::bail!("pinned fake FTPS certificate was not accepted");
        }
    }
}

fn open_ftps(server: &remote::fake_ftp::FakeFtp) -> Result<Session> {
    let spec = spec();
    open_session(
        ftps_connection(server)?,
        &spec,
        RemotePathBuf::new("/").unwrap(),
    )
}

fn ftps_ops(addr: std::net::SocketAddr, certificate: &str) -> Result<Box<dyn RemoteOps>> {
    let settings = TransportSettings {
        protocol: RemoteProtocol::Ftps,
        host: "127.0.0.1".to_owned(),
        port: addr.port(),
        username: "u".to_owned(),
        server_directory: "/".to_owned(),
        authentication: RemoteAuthentication::Password,
        trusted_certificate: Some(certificate.to_owned()),
        ..Default::default()
    };
    match remote::connect(&settings, "pw")? {
        ConnectionAttempt::Connected(conn) => Ok(conn),
        _ => {
            eyre::bail!("pinned fake FTPS certificate was not accepted");
        }
    }
}

/// Deploys `desired` onto the fake host and finishes the operation,
/// leaving an unchanged remote for the phase under test.
fn deploy_initial(
    server: &remote::fake_ftp::FakeFtp,
    publication: &FetchedPublication,
    desired: &DesiredDeployment,
) {
    let addr = server.addr;
    let certificate = server.trusted_certificate().unwrap();
    let mut session = open_ftps(server).unwrap();
    let deployment = deploy(
        &mut session,
        move || ftps_ops(addr, &certificate),
        publication,
        desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();
}

#[test]
fn ftps_preview_recovers_after_reset_without_reupload_or_stranded_lease() {
    use remote::fake_ftp::{FakeFtp, Options};

    let server = FakeFtp::valheim_host(Options {
        tls: true,
        size_requires_binary: true,
        ..Default::default()
    });
    let mut desired = DesiredDeployment::new();
    for index in 0..164 {
        let path = deploy_path(&format!("BepInEx/plugins/Author-ModA/mod-{index:03}.dll"));
        desired.insert(path, staged(format!("mod-{index:03}").as_bytes()));
    }
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let addr = server.addr;
    let certificate = server.trusted_certificate().unwrap();
    let mut deployed = open_ftps(&server).unwrap();
    let deployment = deploy(
        &mut deployed,
        {
            let certificate = certificate.clone();
            move || ftps_ops(addr, &certificate)
        },
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(deployment.summary.uploaded_files, 164);
    finish(
        &mut deployed,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();
    drop(deployed);

    let mut post_deploy = open_ftps(&server).unwrap();
    assert_eq!(post_deploy.state.files.len(), 164);
    // Drop the control connection before the 80th RETR completion.
    // The incomplete hash must be retried on a new FTPS connection.
    server.reset_on_retr(81, false); // state refresh is the first RETR
    let recovered = preview(
        &mut post_deploy,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();
    assert!(recovered.plan.uploads.is_empty());
    assert!(recovered.busy.is_none());
    assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));

    // If the first read of a payload file and its reconnect retry both
    // fail, Preview must return the transport error instead of
    // manufacturing an upload plan. RETRs run on parallel readers now,
    // so the reset targets one path rather than a global ordinal.
    server.reset_on_retr_of("/BepInEx/plugins/Author-ModA/mod-000.dll", 2, false);
    let error = preview(
        &mut post_deploy,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("read failed after reconnect"));
    assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));

    // The in-snapshot state refresh is the authoritative connection's
    // last read before it idles until release: its data and 226 arrive,
    // then the server closes the connection. Release must reconnect,
    // verify the owner, and remove the claim through that fresh
    // connection.
    server.reset_on_retr_of("/BepInEx/config/.gale-server-state.json", 1, true);
    let logins_before = server
        .commands
        .lock()
        .unwrap()
        .iter()
        .filter(|command| command.starts_with("USER "))
        .count();
    let completed = preview(
        &mut post_deploy,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();
    assert!(completed.plan.uploads.is_empty());
    assert!(completed.busy.is_none());
    assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));
    let logins_after = server
        .commands
        .lock()
        .unwrap()
        .iter()
        .filter(|command| command.starts_with("USER "))
        .count();
    assert!(
        logins_after > logins_before,
        "lease release must reconnect after the reset"
    );
    drop(post_deploy);

    let mut subsequent = open_ftps(&server).unwrap();
    let next = preview(
        &mut subsequent,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();
    assert!(next.plan.uploads.is_empty());
    assert!(
        next.busy.is_none(),
        "a successful preview must leave no in-progress warning"
    );
    let unchanged = deploy(
        &mut subsequent,
        {
            let certificate = certificate.clone();
            move || ftps_ops(addr, &certificate)
        },
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        Some(&next.plan.hash),
        false,
    )
    .unwrap();
    assert_eq!(unchanged.summary.uploaded_files, 0);
    finish(
        &mut subsequent,
        unchanged,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();
    assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));
}

/// Counts received `verb` commands by their path argument, e.g.
/// `RETR /BepInEx/plugins/Mod.dll`.
fn command_targets(server: &remote::fake_ftp::FakeFtp, verb: &str) -> BTreeMap<String, usize> {
    let mut targets = BTreeMap::new();
    for command in server.commands.lock().unwrap().iter() {
        if let Some(path) = command.strip_prefix(&format!("{verb} ")) {
            *targets.entry(path.to_owned()).or_default() += 1;
        }
    }
    targets
}

/// An unchanged mods-only preview over FTPS: scanning and hashing run
/// on read-only helper connections, so every payload file and
/// directory is touched exactly once and no payload path pays an MLST
/// probe. The listing already supplied its size.
#[test]
fn ftps_preview_verifies_each_file_and_directory_exactly_once() {
    use remote::fake_ftp::{FakeFtp, Options};

    let server = FakeFtp::valheim_host(Options {
        tls: true,
        size_requires_binary: true,
        ..Default::default()
    });
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = large_mods_payload(50);
    deploy_initial(&server, &publication, &desired);

    let mut session = open_ftps(&server).unwrap();
    server.clear_commands();
    let previewed = preview(
        &mut session,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();
    assert!(previewed.plan.uploads.is_empty());
    assert!(previewed.busy.is_none());
    assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));

    assert_eq!(
        server
            .commands
            .lock()
            .unwrap()
            .iter()
            .filter(|command| command.starts_with("MLST /BepInEx/plugins"))
            .count(),
        0,
        "verification reads must not probe payload metadata"
    );

    let retrs = command_targets(&server, "RETR");
    for path in desired.keys() {
        let remote = format!("/{path}");
        assert_eq!(
            retrs.get(&remote),
            Some(&1),
            "expected exactly one verification read of {remote}"
        );
    }
    // 3 payload roots (plugins, patchers, monomod) + 50 mod dirs +
    // 8 translations dirs.
    let lists = command_targets(&server, "LIST");
    assert_eq!(lists.len(), 61);
    for (dir, count) in &lists {
        assert_eq!(*count, 1, "directory {dir} listed {count} times");
    }

    // Authoritative connection plus at most MAX_SNAPSHOT_READERS
    // helpers; preview runs no heartbeat.
    assert!(
        server.peak_connections() > 1,
        "reader helpers must be opened for this fixture"
    );
    assert!(server.peak_connections() <= 1 + MAX_SNAPSHOT_READERS);
}

/// A same-size edit or deletion between approval and execution
/// must fail the deploy as stale rather than overwrite the remote file.
#[test]
fn ftps_deploy_rejects_payload_drift_after_the_approved_preview() {
    use remote::fake_ftp::{FakeFtp, Options};

    let server = FakeFtp::valheim_host(Options {
        tls: true,
        size_requires_binary: true,
        ..Default::default()
    });
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = large_mods_payload(50);
    deploy_initial(&server, &publication, &desired);
    let addr = server.addr;
    let certificate = server.trusted_certificate().unwrap();

    let drifted = "BepInEx/plugins/Author-Mod00/Mod.dll";
    let drifted_remote = format!("/{drifted}");
    let staged_size = desired[&deploy_path(drifted)].size;

    fn approve(
        session: &mut Session,
        publication: &FetchedPublication,
        desired: &DesiredDeployment,
    ) -> String {
        preview(
            session,
            publication,
            desired,
            &selection(true, false),
            &context(),
            &meta(),
        )
        .unwrap()
        .plan
        .hash
    }
    fn attempt(
        session: &mut Session,
        publication: &FetchedPublication,
        desired: &DesiredDeployment,
        addr: std::net::SocketAddr,
        certificate: &str,
        hash: &str,
    ) -> Result<Deployment> {
        deploy(
            session,
            {
                let certificate = certificate.to_owned();
                move || ftps_ops(addr, &certificate)
            },
            publication,
            desired,
            &selection(true, false),
            &context(),
            &meta(),
            Some(hash),
            false,
        )
    }

    let mut session = open_ftps(&server).unwrap();

    // A same-size remote edit: only hashing the bytes detects it.
    let approved = approve(&mut session, &publication, &desired);
    let replacement = vec![b'!'; staged_size as usize];
    server.seed_file(&drifted_remote, &replacement);
    let error = match attempt(
        &mut session,
        &publication,
        &desired,
        addr,
        &certificate,
        &approved,
    ) {
        Err(error) => error,
        Ok(_) => panic!("a stale approval must be rejected"),
    };
    assert!(
        format!("{error:#}").contains("server state changed"),
        "expected the stale-plan error, got: {error:#}"
    );
    assert_eq!(
        server.file(&drifted_remote),
        Some(replacement),
        "a stale deploy must not overwrite the remote file"
    );
    assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));

    // A deletion between approval and execution is stale too.
    let approved = approve(&mut session, &publication, &desired);
    server.remove_file(&drifted_remote);
    let error = match attempt(
        &mut session,
        &publication,
        &desired,
        addr,
        &certificate,
        &approved,
    ) {
        Err(error) => error,
        Ok(_) => panic!("a stale approval must be rejected"),
    };
    assert!(
        format!("{error:#}").contains("server state changed"),
        "expected the stale-plan error, got: {error:#}"
    );
    assert!(server.file(&drifted_remote).is_none());
    assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));
    // Authoritative + heartbeat slot + readers, across every deploy above.
    assert!(server.peak_connections() <= 2 + MAX_SNAPSHOT_READERS);
}

/// Snapshot helpers are read-only connections: during an unchanged
/// preview every mutating command the fake server receives targets the
/// deployment lease, never a payload or config path.
#[test]
fn ftps_preview_reader_connections_never_mutate_payload_paths() {
    use remote::fake_ftp::{FakeFtp, Options};

    const MUTATING: [&str; 7] = ["STOR", "APPE", "DELE", "MKD", "RMD", "RNFR", "RNTO"];

    let server = FakeFtp::valheim_host(Options {
        tls: true,
        size_requires_binary: true,
        ..Default::default()
    });
    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = large_mods_payload(8);
    deploy_initial(&server, &publication, &desired);

    let mut session = open_ftps(&server).unwrap();
    server.clear_commands();
    let previewed = preview(
        &mut session,
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
    )
    .unwrap();
    assert!(previewed.plan.uploads.is_empty());
    assert!(
        server.peak_connections() > 1,
        "reader helpers must be opened for this test to prove anything"
    );

    let mut saw_mutation = false;
    for command in server.commands.lock().unwrap().iter() {
        let verb = command.split(' ').next().unwrap_or_default();
        if MUTATING.contains(&verb) {
            saw_mutation = true;
            let target = command.split(' ').nth(1).unwrap_or_default();
            assert!(
                target.starts_with("/BepInEx/config/.gale-deploy.lock"),
                "{command} mutated a path outside the deployment lease"
            );
        }
    }
    assert!(saw_mutation, "lease acquire/release must have run");
}
/// dot-prefixed paths fully visible, exercised through the real FTP
/// transport. A completed deployment must write, verify, and reload
/// its exact recorded state through a fresh connection, and a second
/// operation must remove an owned file while preserving unmanaged
/// ones.
#[test]
fn deployment_state_round_trips_on_a_size_refusing_host() {
    use remote::fake_ftp::{FakeFtp, Options};

    let server = FakeFtp::valheim_host(Options {
        size_requires_binary: true,
        ..Default::default()
    });
    // An unmanaged file inside the payload tree must never be removed.
    server.seed_file("/BepInEx/plugins/hand-placed.dll", b"foreign");
    let addr = server.addr;

    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = desired_payload();

    let mut session = open_ftp(addr).unwrap();
    let deployment = deploy(
        &mut session,
        move || ftp_ops(addr),
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(deployment.summary.uploaded_files, 1);
    let first = finish(
        &mut session,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();
    assert_eq!(first.operation_seq, 1);
    drop(session);

    // A fresh connection reloads the exact recorded state that a
    // worker executor would see.
    let mut second = open_ftp(addr).unwrap();
    assert_eq!(
        state::serialize(&second.state).unwrap(),
        state::serialize(&first).unwrap(),
        "a fresh session must reload the persisted state exactly"
    );
    assert!(
        second
            .state
            .files
            .contains_key(&deploy_path("BepInEx/plugins/Author-ModA/ModA.dll"))
    );

    // Second operation over another connection: the owned mod is
    // removed, the unmanaged file survives, the sequence advances.
    let empty = Fixture {
        mods: Vec::new(),
        config: BTreeMap::new(),
    };
    let deployment = deploy(
        &mut second,
        move || ftp_ops(addr),
        &empty.publication(),
        &DesiredDeployment::new(),
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .unwrap();
    assert_eq!(deployment.summary.removed_files, 1);
    let second_state = finish(
        &mut second,
        deployment,
        RestartOutcome::NotRequired,
        &meta(),
    )
    .unwrap();
    assert_eq!(second_state.operation_seq, 2);

    assert!(
        server
            .file("/BepInEx/plugins/Author-ModA/ModA.dll")
            .is_none()
    );
    assert_eq!(
        server.file("/BepInEx/plugins/hand-placed.dll"),
        Some(b"foreign".to_vec())
    );
    assert!(
        server
            .file("/BepInEx/config/.gale-server-state.json")
            .is_some()
    );
    assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));
}

/// A host that refuses to return even freshly-written state cannot
/// prove persistence. The deployment must fail closed. The holder
/// marker still verifies the lease (its record is equally refused),
/// and the claim is released cleanly.
#[test]
fn deploy_fails_when_the_state_cannot_be_verified() {
    use remote::fake_ftp::{FakeFtp, Options};

    let server = FakeFtp::valheim_host(Options {
        refuse_retr: true,
        ..Default::default()
    });
    let addr = server.addr;

    let fixture = mod_fixture();
    let publication = fixture.publication();
    let desired = desired_payload();

    let mut session = open_ftp(addr).unwrap();
    let err = deploy(
        &mut session,
        move || ftp_ops(addr),
        &publication,
        &desired,
        &selection(true, false),
        &context(),
        &meta(),
        None,
        false,
    )
    .err()
    .expect("unverifiable state persistence must fail the deploy");

    assert!(
        format!("{err:#}").contains("failed to persist remote deployment state"),
        "expected a persistence failure, got: {err:#}"
    );
    // The marker proved ownership even though the lease record could
    // not be read back either.
    assert!(
        session
            .warnings
            .iter()
            .any(|w| w.contains("marker directory"))
    );
    // The upload landed and the lease was released. The state temp
    // was written but could never be verified, so it was never
    // renamed. An unverifiable write must not become the
    // authoritative state.
    assert!(
        server
            .file("/BepInEx/plugins/Author-ModA/ModA.dll")
            .is_some()
    );
    assert!(!server.has_dir("/BepInEx/config/.gale-deploy.lock"));
    assert!(
        server
            .file("/BepInEx/config/.gale-server-state.json")
            .is_none()
    );
    assert!(
        server
            .file("/BepInEx/config/.gale-server-state.json.tmp")
            .is_some()
    );
}

/// A remote state that moved under this session is never overwritten:
/// the sequence guard fires over the real transport too.
#[test]
fn persist_state_refuses_to_overwrite_a_moved_remote() {
    use remote::fake_ftp::FakeFtp;

    let server = FakeFtp::valheim_host(Default::default());
    let mut session = open_ftp(server.addr).unwrap();

    // Another writer moved the remote state after this session loaded it.
    let moved = serde_json::json!({"version": 2, "operationSeq": 7});
    server.seed_file(
        "/BepInEx/config/.gale-server-state.json",
        &serde_json::to_vec(&moved).unwrap(),
    );

    let err =
        persist_state(&mut session).expect_err("a moved remote sequence must refuse the write");
    assert!(
        format!("{err:#}").contains("changed during this operation"),
        "expected the sequence guard, got: {err:#}"
    );
    // And nothing was written over the newer state.
    assert_eq!(
        server.file("/BepInEx/config/.gale-server-state.json"),
        Some(serde_json::to_vec(&moved).unwrap())
    );
}

#[test]
fn authoritative_state_read_reconnects_before_persisting_exact_bytes() {
    use remote::fake_ftp::{FakeFtp, Options};

    let server = FakeFtp::valheim_host(Options {
        tls: true,
        ..Default::default()
    });
    let target = "/BepInEx/config/.gale-server-state.json";
    let initial = ServerDeploymentState {
        version: state::VERSION,
        ..Default::default()
    };
    server.seed_file(target, &state::serialize(&initial).unwrap());
    let mut session = open_ftps(&server).unwrap();
    session.state.restart_required = true;
    let expected = state::serialize(&session.state).unwrap();

    // The first RETR is the pre-write authoritative read. A lost
    // completion response must not prevent its safe fresh-connection retry.
    server.reset_on_retr(1, false);
    persist_state(&mut session).unwrap();
    let mut fresh = ftps_connection(&server).unwrap();
    assert_eq!(
        fresh
            .read(&RemotePathBuf::new(target).unwrap(), state::MAX_STATE_BYTES)
            .unwrap(),
        Some(expected)
    );

    // A changed sequence still defeats the guard after reconnection.
    server.seed_file(target, br#"{"version":2,"operationSeq":4}"#);
    server.reset_on_retr(1, false);
    let error = persist_state(&mut session).unwrap_err();
    assert!(format!("{error:#}").contains("changed during this operation"));
}

/// A store that truncates the state temp file but still answers `226`
/// fails the read-back check, and that failure must not have already
/// destroyed the previously valid state. A fresh FTPS connection
/// proves the exact original bytes survive.
#[test]
fn persist_state_never_replaces_valid_state_with_a_truncated_temp() {
    use remote::fake_ftp::{FakeFtp, Options};

    let server = FakeFtp::valheim_host(Options {
        tls: true,
        truncate_stor_to: Some(512),
        ..Default::default()
    });
    let original = ServerDeploymentState {
        version: state::VERSION,
        ..Default::default()
    };
    let original_bytes = state::serialize(&original).unwrap();
    let target = "/BepInEx/config/.gale-server-state.json";
    server.seed_file(target, &original_bytes);

    let mut session = open_ftps(&server).unwrap();
    for index in 0..64 {
        let path = deploy_path(&format!("BepInEx/plugins/Owned-{index}/file.dll"));
        session.state.files.insert(
            path,
            OwnedFile {
                hash: ContentHash::from_hash(blake3::hash(format!("owned-{index}").as_bytes())),
                size: index as u64,
            },
        );
    }
    assert!(state::serialize(&session.state).unwrap().len() > 512);

    let error = persist_state(&mut session).expect_err("truncated temp must fail persistence");
    assert!(
        format!("{error:#}").contains("failed to write remote deployment state"),
        "expected truncated state persistence to fail, got: {error:#}"
    );

    let mut fresh = ftps_connection(&server).unwrap();
    let actual = fresh
        .read(&RemotePathBuf::new(target).unwrap(), state::MAX_STATE_BYTES)
        .unwrap()
        .expect("the prior state must remain");
    assert_eq!(actual.len(), original_bytes.len());
    assert_eq!(blake3::hash(&actual), blake3::hash(&original_bytes));
    assert_eq!(actual, original_bytes);
}

/// The success path of the same contract: a verifiable state write
/// lands byte-identical on the target, proven through an independent
/// FTPS connection rather than the fake's filesystem shortcut.
#[test]
fn persist_state_round_trips_exact_bytes_across_fresh_ftps_connections() {
    use remote::fake_ftp::{FakeFtp, Options};

    let server = FakeFtp::valheim_host(Options {
        tls: true,
        ..Default::default()
    });
    let mut session = open_ftps(&server).unwrap();
    for index in 0..64 {
        let path = deploy_path(&format!("BepInEx/plugins/Owned-{index}/file.dll"));
        session.state.files.insert(
            path,
            OwnedFile {
                hash: ContentHash::from_hash(blake3::hash(format!("owned-{index}").as_bytes())),
                size: index as u64,
            },
        );
    }
    let expected = state::serialize(&session.state).unwrap();

    persist_state(&mut session).unwrap();
    drop(session);

    let target = RemotePathBuf::new("/BepInEx/config/.gale-server-state.json").unwrap();
    let mut fresh = ftps_connection(&server).unwrap();
    let actual = fresh
        .read(target.as_path(), state::MAX_STATE_BYTES)
        .unwrap()
        .expect("persisted state must exist");
    assert_eq!(actual.len(), expected.len());
    assert_eq!(blake3::hash(&actual), blake3::hash(&expected));
    assert_eq!(actual, expected);
    assert!(
        server
            .file("/BepInEx/config/.gale-server-state.json.tmp")
            .is_none()
    );
    let auth_tls_count = server
        .commands
        .lock()
        .unwrap()
        .iter()
        .filter(|line| line.as_str() == "AUTH TLS")
        .count();
    assert!(
        auth_tls_count >= 3,
        "write, temporary verification, final verification, and test read must use fresh FTPS sessions"
    );
}

/// A persistent config policy set under the lease survives a full
/// reconnect because decisions are part of the durable state.
#[test]
fn a_config_policy_persists_across_reconnects_over_ftp() {
    use remote::fake_ftp::{FakeFtp, Options};

    let server = FakeFtp::valheim_host(Options::default());
    let addr = server.addr;
    let path = config_path("BepInEx/config/test.cfg");

    let mut session = open_ftp(addr).unwrap();
    set_config_policy(
        &mut session,
        &path,
        ConfigUpdatePolicy::AlwaysKeep,
        None,
        &meta(),
    )
    .unwrap();
    drop(session);

    let second = open_ftp(addr).unwrap();
    assert_eq!(
        second.state.config.get(&path).map(|r| r.policy),
        Some(ConfigUpdatePolicy::AlwaysKeep)
    );
}
