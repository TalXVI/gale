//! The deployment service both executors run. Desktop Local mode calls it
//! in-process and the worker calls it from its HTTP handlers and poll
//! loop, so connecting, staging, plan binding, and every remote operation
//! exist once. Callers supply only what genuinely differs between them:
//! where the publication comes from, which restart policy is current,
//! where progress goes, and their own bookkeeping.

use std::path::PathBuf;

use eyre::{Context, Result, bail};
use reqwest_middleware::ClientWithMiddleware;

use super::{
    engine::{self, DeploymentResult, OperationMeta, Preview, Session},
    host::HostControl,
    plan::{DeploySelection, DesiredDeployment, PlanContext},
    progress::ProgressReporter,
    remote::{self, ConnectionAttempt, RemoteOps},
    settings::{RestartPolicy, TransportSettings},
    spec::DeploymentSpec,
    stage,
    state::ServerDeploymentState,
};
use crate::{
    game::mod_loader::ModLoader,
    profile::{
        export::{ConfigPath, ContentHash},
        sync::{ConfigUpdatePolicy, FetchedPublication},
    },
};

/// How a refused connection tells the user to pin the server's identity,
/// which depends on where the executor's settings live.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PinAdvice {
    pub(crate) host_key: &'static str,
    pub(crate) certificate: &'static str,
}

/// Opens authenticated connections to one remote server.
#[derive(Clone)]
pub(crate) struct Connector {
    pub(crate) settings: TransportSettings,
    pub(crate) password: String,
    pub(crate) pin_advice: PinAdvice,
}

impl Connector {
    /// Connects to the remote server, failing on untrusted keys rather than
    /// silently sending credentials.
    pub(crate) fn connect(&self) -> Result<Box<dyn RemoteOps>> {
        match remote::connect(&self.settings, &self.password)? {
            ConnectionAttempt::Connected(connection) => Ok(connection),
            ConnectionAttempt::HostKeyUntrusted { fingerprint } => {
                bail!(
                    "SFTP host key is not trusted ({fingerprint}); {}",
                    self.pin_advice.host_key
                );
            }
            ConnectionAttempt::CertificateUntrusted { fingerprint } => {
                bail!(
                    "FTPS certificate is not trusted ({fingerprint}); {}",
                    self.pin_advice.certificate
                );
            }
        }
    }
}

/// One executor's binding to a profile, its game, and its remote server.
pub(crate) struct DeployService {
    pub(crate) connector: Connector,
    pub(crate) spec: DeploymentSpec,
    /// The profile identity a plan approval is bound to.
    pub(crate) profile_id: String,
    /// The game slug a plan approval is bound to.
    pub(crate) game: String,
    pub(crate) mod_loader: &'static ModLoader<'static>,
    pub(crate) cache_dir: PathBuf,
    /// Downloads mod packages for staging.
    pub(crate) http: ClientWithMiddleware,
}

impl DeployService {
    /// The context the plan hash binds an approval to: the profile, game,
    /// remote identity, and the restart policy the operation will apply.
    pub(crate) fn plan_context(&self, restart_policy: RestartPolicy) -> PlanContext {
        PlanContext {
            profile_id: self.profile_id.clone(),
            game: self.game.clone(),
            target: self.connector.settings.describe_target(),
            restart_policy,
        }
    }

    /// Opens a session and runs blocking work on it behind the async
    /// boundary.
    pub(crate) async fn with_session<T: Send + 'static>(
        &self,
        work: impl FnOnce(Session) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let connector = self.connector.clone();
        let spec = self.spec.clone();
        let base = self.connector.settings.server_directory()?;
        tokio::task::spawn_blocking(move || {
            work(engine::open_session(connector.connect()?, &spec, base)?)
        })
        .await
        .context("remote operation task failed")?
    }

    /// A live read of the remote deployment state.
    pub(crate) async fn read_session(&self) -> Result<Session> {
        self.with_session(Ok).await
    }

    /// Stages the publication's payload when the selection includes mods.
    async fn stage(
        &self,
        publication: &FetchedPublication,
        include_mods: bool,
        progress: &mut ProgressReporter,
    ) -> Result<DesiredDeployment> {
        stage::stage_fetched(
            publication,
            self.cache_dir.clone(),
            self.http.clone(),
            self.mod_loader,
            &self.spec,
            include_mods,
            progress,
        )
        .await
        .context("failed to stage the published mod set")
    }

    /// Stages `publication` and plans `selection` against the live remote.
    pub(crate) async fn preview(
        &self,
        publication: FetchedPublication,
        selection: DeploySelection,
        restart_policy: RestartPolicy,
        meta: OperationMeta,
        progress: &mut ProgressReporter,
    ) -> Result<Preview> {
        let desired = self
            .stage(&publication, selection.include_mods, progress)
            .await?;
        let connector = self.connector.clone();
        engine::preview(
            move || connector.connect(),
            self.spec.clone(),
            self.connector.settings.server_directory()?,
            publication,
            desired,
            selection,
            self.plan_context(restart_policy),
            meta,
            progress,
        )
        .await
    }

    /// Stages `publication` and deploys `selection`, then applies
    /// `restart_policy` through `host`. A `plan_hash` binds the deployment
    /// to an approved preview.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn deploy(
        &self,
        publication: FetchedPublication,
        selection: DeploySelection,
        plan_hash: Option<String>,
        force: bool,
        restart_policy: RestartPolicy,
        host: &dyn HostControl,
        meta: OperationMeta,
        progress: &mut ProgressReporter,
    ) -> Result<DeploymentResult> {
        let desired = self
            .stage(&publication, selection.include_mods, progress)
            .await?;
        let connector = self.connector.clone();
        engine::deploy(
            move || connector.connect(),
            self.spec.clone(),
            self.connector.settings.server_directory()?,
            publication,
            desired,
            selection,
            self.plan_context(restart_policy),
            meta,
            plan_hash,
            force,
            host,
            restart_policy,
            progress,
        )
        .await
    }

    /// Rechecks the restart requested by an earlier deployment without
    /// fetching a publication or touching its payload and runtime configs.
    #[cfg(feature = "worker")]
    pub(crate) async fn resume_deferred_restart(
        &self,
        expected_operation_id: String,
        host: &dyn HostControl,
        meta: OperationMeta,
        progress: &mut ProgressReporter,
    ) -> Result<ServerDeploymentState> {
        let session = self.read_session().await?;
        let connector = self.connector.clone();
        engine::resume_deferred_restart(
            session,
            move || connector.connect(),
            expected_operation_id,
            host,
            meta,
            progress,
        )
        .await
    }

    /// Sets a persistent per-file update policy, pinned at the canonical
    /// publication's hash for that file.
    pub(crate) async fn set_config_policy(
        &self,
        path: ConfigPath,
        policy: ConfigUpdatePolicy,
        pinned_at: Option<ContentHash>,
        meta: OperationMeta,
    ) -> Result<()> {
        self.with_session(move |mut session| {
            engine::set_config_policy(&mut session, &path, policy, pinned_at.as_ref(), &meta)
        })
        .await
    }

    /// Records the user's confirmation that the server was restarted
    /// outside Gale.
    pub(crate) async fn acknowledge_external_restart(
        &self,
        meta: OperationMeta,
    ) -> Result<ServerDeploymentState> {
        self.with_session(move |mut session| {
            engine::acknowledge_external_restart(&mut session, &meta)
        })
        .await
    }
}
