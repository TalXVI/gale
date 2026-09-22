export type ConfigValueType<T extends string, C> = { type: T; content: C };

export type ConfigValue =
	| ConfigValueType<'bool', boolean>
	| ConfigValueType<'string', string>
	| ConfigValueType<'int', ConfigNum>
	| ConfigValueType<'float', ConfigNum>
	| ConfigValueType<'enum', { index: number; options: string[] }>
	| ConfigValueType<'flags', { indicies: number[]; options: string[] }>;

export type ConfigEntry = {
	name: string;
	description: string | null;
	default: ConfigValue | null;
	value: ConfigValue;
};

export type ConfigSection = {
	name: string;
	entries: ConfigEntry[];
};

export type ConfigFileData = {
	displayName: string;
	relativePath: string;
	sections: ConfigSection[];
	metadata: ConfigFileMetadata | null;
};

export type ConfigFileMetadata = {
	modName: string;
	modVersion: string;
};

export type ConfigNum = {
	value: number;
	range: ConfigRange | null;
};

export type ConfigRange = {
	start: number;
	end: number;
};

export type ConfigFileType<T extends string, C = {}> = { type: T } & C;

export type BaseConfigFile = { relativePath: string; displayName: string | null };

export type ConfigFile = BaseConfigFile &
	(
		| ConfigFileType<'ok', ConfigFileData>
		| ConfigFileType<'err', { error: string }>
		| ConfigFileType<'unsupported'>
	);

export type ProfileInfo = {
	id: number;
	name: string;
	modCount: number;
	sync: SyncProfileInfo | null;
	customArgs: string;
	missing: boolean;
};

export type SyncProfileInfo = {
	id: string;
	owner: SyncUser;
	syncedAt: string;
	updatedAt: string;
	missing: boolean;
};

export type ListedSyncProfile = {
	id: string;
	name: string;
	community: string;
	createdAt: string;
	updatedAt: string;
};

export type SyncUser = {
	discordId: string;
	name: string;
	displayName: string;
	avatar: string | null;
};

export type SyncPublishMode =
	| { kind: 'mods' }
	| { kind: 'config'; files: string[] }
	| { kind: 'both'; files: string[] };

export type SyncConfigFileStatus = 'new' | 'modified' | 'published';

export type SyncConfigFileInfo = {
	path: string;
	size: number;
	status: SyncConfigFileStatus;
};

export type PendingSyncConfigReason = 'modifiedLocally' | 'deletedLocally';

export type SyncConfigUpdatePolicy = 'ask' | 'alwaysApply' | 'alwaysKeep';

export type SyncConfigReviewItem = {
	path: string;
	reason: PendingSyncConfigReason;
};

export type SyncConfigPolicyEntry = {
	path: string;
	policy: SyncConfigUpdatePolicy;
};

export type SyncConfigReviewState = {
	pending: SyncConfigReviewItem[];
	declined: SyncConfigReviewItem[];
	policies: SyncConfigPolicyEntry[];
};

export type SyncConfigApplyReport = {
	installed: string[];
	pending: SyncConfigReviewItem[];
};

export type ManagedGameInfo = {
	profiles: ProfileInfo[];
	activeId: number;
};

export type GameInfo = {
	active: Game;
	lastUpdated: string;
	all: Game[];
	favorites: string[];
};

export enum Backend {
	Thunderstore = 'Thunderstore',
	Hexium = 'Hexium'
}

export type Mod = {
	name: string;
	description: string | null;
	categories: string[] | null;
	version: string | null;
	author: string | null;
	rating: number | null;
	downloads: number | null;
	fileSize: number;
	websiteUrl: string | null;
	donateUrl: string | null;
	dependencies: string[] | null;
	suggestions: string[] | null;
	isPinned: boolean;
	isDeprecated: boolean;
	isInstalled: boolean | undefined;
	containsNsfw: boolean;
	uuid: string;
	versionUuid: string;
	lastUpdated: string | null;
	versions: ModVersion[];
	type: ModType;
	enabled?: boolean | null;
	icon: string | null;
	configFile: string | null;
	backend: Backend;
};

export type ModVersion = {
	name: string;
	uuid: string;
};

export enum ModType {
	Local = 'local',
	Remote = 'remote'
}

export type SortBy =
	| 'newest'
	| 'name'
	| 'author'
	| 'lastUpdated'
	| 'downloads'
	| 'rating'
	| 'installDate'
	| 'custom'
	| 'diskSpace';

export type SortOrder = 'ascending' | 'descending';

export type QueryModsArgs = {
	searchTerm: string;
	includeCategories: string[];
	excludeCategories: string[];
	includeNsfw: boolean;
	includeDeprecated: boolean;
	includeDisabled: boolean;
	includeEnabled: boolean;
	sortBy: SortBy;
	sortOrder: SortOrder;
	maxCount: number | null;
};

export type QueryModsArgsWithoutMax = Omit<QueryModsArgs, 'maxCount'>;

export type ConfigEntryId = {
	file: { relativePath: string };
	section: ConfigSection;
	entry: ConfigEntry;
};

export type Dependant = {
	fullName: string;
	uuid: string;
	backend: Backend;
};

export type DependantWithVersion = {
	fullName: string;
	preferredVersion: string | null;
	backend: Backend;
};

export type ModId = {
	packageUuid: string;
	versionUuid: string;
	backend: Backend;
};

export type ModActionResponse =
	| { type: 'done' }
	| { type: 'hasDependants'; dependants: Dependant[] };

export type InstallTask = 'download' | 'extract' | 'install';

export type InstallEvent =
	| { type: 'show' }
	| { type: 'hide'; reason: 'done' | 'error' | 'cancelled' }
	| { type: 'addCount'; mods: number; bytes: number }
	| { type: 'addProgress'; mods: number; bytes: number }
	| { type: 'setTask'; name: string; task: InstallTask };

export type FetchEvent =
	| { type: 'start'; backend: Backend }
	| { type: 'progress'; backend: Backend; mods: number }
	| { type: 'done'; backend: Backend };

export type ModpackArgs = {
	name: string;
	description: string;
	author: string;
	categories: string[];
	nsfw: boolean;
	readme: string;
	changelog: string;
	versionNumber: string;
	iconPath: string;
	websiteUrl: string;
	includeDisabled: boolean;
	includeFileMap: Map<string, boolean>;
	backend: Backend;
};

export type ModpackInfo = {
	args: ModpackArgs;
	hexiumExclusive: boolean;
};

export type ExportCode = {
	code: string;
	backend: Backend;
};

export type DedicatedServerInfo = {
	platforms: Platform[];
	defaultPort: number;
};

export type ServerLocation = 'local' | 'remote';
export type RemoteAuthentication = 'password' | 'privateKey' | 'agent';
export type RemoteProtocol = 'sftp' | 'ftp' | 'ftps';
export type SyncMode = 'local' | 'worker';
export type RestartPolicy = 'manual' | 'immediate' | 'whenEmpty';
export type HostProvider = 'none' | 'datHost';

export type WorkerSettings = {
	address: string;
	/// True when `address` points at the Gale-managed Windows service on
	/// this machine rather than an independently hosted worker.
	hosted: boolean;
	autoSync: boolean;
	autoMods: boolean;
};

export type HostSettings = {
	provider: HostProvider;
	datHostServerId: string;
	datHostUsername: string;
};

export type RemoteServerSettings = {
	protocol: RemoteProtocol;
	host: string;
	port: number;
	username: string;
	serverDirectory: string;
	authentication: RemoteAuthentication;
	privateKeyPath: string;
	trustedHostKey: string | null;
	trustedCertificate: string | null;
	syncMode: SyncMode;
	worker: WorkerSettings;
	hostControl: HostSettings;
	restartPolicy: RestartPolicy;
};

export type ProfileServerSettings = {
	location: ServerLocation;
	serverName: string;
	world: string;
	port: number;
	publicServer: boolean;
	crossplay: boolean;
	extraArgs: string;
	remote: RemoteServerSettings;
};

export type RemoteConnectionTestResult =
	| { status: 'connected'; fingerprint: string | null; encrypted: boolean }
	| { status: 'hostKeyUntrusted'; fingerprint: string }
	| { status: 'certificateUntrusted'; fingerprint: string };

// ---------- selective server synchronization ----------

export type DeploySelection = {
	includeMods: boolean;
	includeConfigs: boolean;
	applyConfigs: string[];
	restoreConfigs: string[];
	declineConfigs: string[];
};

export type UploadKind = 'payload' | 'configSeed' | 'config';
export type RemoteLayout = 'standard' | 'mirrorRoot';

export type PlanUpload = {
	path: string;
	size: number;
	kind: UploadKind;
};

export type ConfigAction =
	| { action: 'markApplied' }
	| { action: 'write' }
	| { action: 'decline' }
	| { action: 'keep' }
	| { action: 'pending'; reason: PendingSyncConfigReason }
	| { action: 'unapplied' };

export type PlanConfigEntry = {
	path: string;
	policy: SyncConfigUpdatePolicy;
	selected: boolean;
} & ConfigAction;

export type PlanConflict = {
	path: string;
	reason: PendingSyncConfigReason;
	seed: boolean;
};

export type DeploymentPlan = {
	hash: string;
	publicationRevision: string;
	modsRevision: string;
	deployedModsRevision: string | null;
	stateSeq: number;
	layout: RemoteLayout;
	hostManaged: boolean;
	modsPhase: boolean;
	configsPhase: boolean;
	uploads: PlanUpload[];
	uploadBytes: number;
	removals: string[];
	directoryRemovals: string[];
	unchangedFiles: number;
	configEntries: PlanConfigEntry[];
	conflicts: PlanConflict[];
	/// Remote payload files Gale has never owned. Shown for review,
	/// never deleted by the deployment.
	unmanaged: string[];
	requiresRestart: boolean;
};

export type ExecutorKind = 'local' | 'worker';
export type OperationKind = 'manual' | 'automatic';
export type OperationStatus = 'succeeded' | 'partial' | 'failed';
export type RestartOutcome =
	| 'notRequired'
	| 'awaitingManual'
	| 'awaitingEmpty'
	| 'restarted'
	| 'startupUnverified'
	| 'failed';

export type OperationSummary = {
	uploadedFiles: number;
	uploadedBytes: number;
	removedFiles: number;
	configWrites: number;
	unchangedFiles: number;
};

export type OperationRecord = {
	id: string;
	executor: ExecutorKind;
	kind: OperationKind;
	workerId: string | null;
	publicationRevision: string | null;
	modsRevision: string | null;
	status: OperationStatus;
	summary: OperationSummary;
	restart: RestartOutcome;
	error: string | null;
	startedAt: string;
	finishedAt: string;
};

export type LeaseRecord = {
	owner: string;
	executor: ExecutorKind;
	operationId: string;
	acquiredAt: string;
	heartbeatAt: string;
	ttlSecs: number;
};

/// A preview that found another live executor holding the deployment
/// lease. `stale` marks a lease whose heartbeat expired, the only case
/// where a forced takeover is offered.
export type LeaseBusy = {
	record: LeaseRecord;
	stale: boolean;
};

/// The subset of the remote deployment state the UI displays.
export type ServerDeploymentState = {
	version: number;
	operationSeq: number;
	modsRevision: string | null;
	restartRequired: boolean;
	pending: Record<string, PendingSyncConfigReason>;
	lastOperation: OperationRecord | null;
};

export type ServerSyncPreview = {
	plan: DeploymentPlan;
	busy: LeaseBusy | null;
	warnings: string[];
};

export type ServerSyncResult = {
	plan: DeploymentPlan;
	summary: OperationSummary;
	warnings: string[];
	failedConfigWrites: string[];
	restart: RestartOutcome;
	state: ServerDeploymentState;
};

export type BusyOperation = {
	id: string;
	kind: OperationKind;
	startedAt: string;
};

export type ServerStateSummary = {
	modsRevision: string | null;
	restartRequired: boolean;
	pendingConfigs: number;
	lastOperation: OperationRecord | null;
	lease: LeaseRecord | null;
};

export type WorkerStatus = {
	workerId: string;
	profileId: string;
	autoSync: boolean;
	autoMods: boolean;
	restartPolicy: RestartPolicy;
	/// The newest publication revision the worker has observed.
	/// Observation alone is not deployment.
	observedRevision: string | null;
	/// A publication revision awaiting successful deployment, if any.
	pendingRevision: string | null;
	/// When the pending work becomes eligible for its next attempt.
	nextAttemptAt: string | null;
	/// The newest publication revision that fully deployed successfully.
	lastDeployedRevision: string | null;
	busy: BusyOperation | null;
	lastOperation: OperationRecord | null;
	lastError: string | null;
	server: ServerStateSummary | null;
};

export type ServerSyncStatus = {
	mode: SyncMode;
	server: ServerStateSummary | null;
	publicationRevision: string | null;
	worker: WorkerStatus | null;
	credentialRequired: boolean;
	warnings: string[];
};

// ---------- managed local worker ("host worker on this PC") ----------

export type LocalWorkerServiceState =
	| 'notInstalled'
	| 'stopped'
	| 'startPending'
	| 'running'
	| 'stopPending'
	| 'other';

export type LocalWorkerBinding = {
	workerId: string;
	/// The sync-profile id the installed worker serves.
	profileId: string;
	listen: string;
	address: string;
};

/// Why the worker process last stopped, from its status file. A `running`
/// report on a stopped service means the process died unexpectedly.
export type LocalWorkerRunPhase = 'running' | 'stopped' | 'shutdown';

export type LocalWorkerRunReport = {
	workerId: string;
	profileId: string;
	pid: number;
	phase: LocalWorkerRunPhase;
	at: string;
};

export type LocalWorkerAction = 'start' | 'stop' | 'restart';

/// Who the installed worker belongs to relative to this profile. One
/// `GaleWorker` service exists per machine and is bound to a single sync
/// profile at install — `foreign` workers must never be controlled from
/// here.
export type LocalWorkerOwnership =
	/// No worker is installed.
	| 'none'
	/// Installed for this profile and fully bound (settings + token).
	| 'owned'
	/// Installed for this profile but the desktop binding never completed
	/// — provisioning can finish it.
	| 'incomplete'
	/// Installed for a different profile; controls must not be offered.
	| 'foreign';

/// What the worker will do with its pending publication, derived from
/// its own reported automation flags — not the unsaved local checkboxes.
export type PendingPublicationMode = 'automatic' | 'configOnly' | 'manual';

export type PendingPublication = {
	mode: PendingPublicationMode;
	/// A previous automatic attempt failed; a retry is scheduled.
	retrying: boolean;
};

export type LocalWorkerStatus = {
	/// False off Windows — provisioning is unavailable there.
	supported: boolean;
	service: LocalWorkerServiceState;
	binding: LocalWorkerBinding | null;
	/// Whether the installed worker belongs to this profile.
	ownership: LocalWorkerOwnership;
	run: LocalWorkerRunReport | null;
	worker: WorkerStatus | null;
	/// The pending banner's content; null hides it — nothing pending, or
	/// the worker is stopped/unreachable so pending state is unknown.
	pendingPublication: PendingPublication | null;
	workerError: string | null;
	/// The last run report says the machine shut down — the service
	/// comes back with the next boot.
	stoppedForShutdown: boolean;
	/// A newer worker binary shipped with the app than the service runs.
	updateAvailable: boolean;
	warnings: string[];
};

export type ServerSyncProgress = {
	completed: number;
	total: number;
	path: string;
	operation: 'remove' | 'upload' | 'writeConfig';
};

export type ServerSyncStageProgress = {
	completed: number;
	total: number;
	mod: string;
};

export type DedicatedServerStatus =
	| { state: 'stopped' }
	| {
		state: 'running';
		profileId: number;
		gameSlug: string;
		pid: number;
		serverDir: string;
	};

export type Game = {
	name: string;
	slug: string;
	platforms: Platform[];
	favorite: boolean;
	modLoader: ModLoader;
	popular: boolean;
	backends: Backend[];
	dedicatedServer: DedicatedServerInfo | null;
};

export enum ModLoader {
	BepInEx = 'BepInEx',
	MelonLoader = 'MelonLoader',
	Northstar = 'Northstar',
	GDWeave = 'GDWeave',
	ReturnOfModding = 'ReturnOfModding',
	BepisLoader = 'BepisLoader',
	Shimloader = 'Shimloader',
	Lovely = 'Lovely'
}

export type PackageCategory = {
	name: string;
	slug: string;
};

export type FiltersResponse = {
	results: PackageCategory[];
};

export type LaunchMode =
	| { type: 'launcher'; content?: undefined }
	| { type: 'direct'; content: { instances: number; intervalSecs: number } };

export type AvailableUpdate = {
	fullName: string;
	ignore: boolean;
	isCrossBackend: boolean;
	updatedId: ModId;
	old: string;
	new: string;
};

export type ProfileQuery = {
	mods: Mod[];
	totalModCount: number;
	unknownMods: Dependant[];
	updates: AvailableUpdate[];
};

export type ImportData =
	| ({ type: 'legacy' } & LegacyImportData)
	| ({ type: 'sync' } & SyncImportData);

export type LegacyImportData = {
	manifest: ProfileManifest;
	path: string;
	deleteAfterImport: boolean;
	missingMods: string[];
};

export type SyncImportData = {
	manifest: ProfileManifest;
	id: string;
	created_at: string;
	updated_at: string;
	owner: SyncUser;
};

type ProfileManifest = {
	profileName: string;
	mods: ProfileManifestMod[];
	community: string | null;
	ignoredUpdates: string[];
};

type ProfileManifestMod = {
	name: string;
	enabled: string;
	version: {
		major: number;
		minor: number;
		patch: number;
		pre?: string;
		build?: string;
	};
	source: Backend;
};

export type R2ImportData = {
	path: string;
	profiles: string[];
	include: boolean[];
};

export type ImportOptions = {
	importAll?: boolean;
	merge?: boolean;
};

export type UploadSubmissionResult = {
	hidden?: boolean;
};

export type Prefs = {
	dataDir: string;
	cacheDir: string;
	fetchModsAutomatically: boolean;
	pullBeforeLaunch: boolean;
	zoomFactor: number;
	language: string;
	gamePrefs: Map<string, GamePrefs>;
	backendSkipConfirm: boolean;
};

export enum Backends {
	All = 'All',
	Thunderstore = 'Thunderstore',
	Hexium = 'Hexium'
}

export type GamePrefs = {
	dirOverride: string | null;
	customArgs: string;
	launchMode: LaunchMode;
	platform: Platform | null;
	showSteamLaunchOptions: boolean;
	backend: Backends;
};

export type Platform = 'steam' | 'epicGames' | 'oculus' | 'origin' | 'xboxStore';

export type ContextItem = {
	label: string;
	icon?: string;
	onclick: () => void;
	children?: ContextItem[];
};

export type ModContextItem = {
	label: string;
	icon?: string;
	showFor?: (mod: Mod, locked: boolean) => boolean;
	onclick: (mod: Mod) => void;
	children?: (mod: Mod) => ModContextItem[];
};

export type Zoom = { factor: number } | { delta: number };

export type MarkdownType = 'readme' | 'changelog';

export interface LaunchOption {
	arguments: string;
	type: string | null;
	description: string | null;
}
export type MissingProfileAction = { type: 'locate'; newPath: string } | { type: 'delete' };

export type Folder = {
	id: string;
	children: ListItem[];
};

export type ListItem =
	| {
		type: 'mod';
		mod: Mod;
	}
	| {
		type: 'folder';
		folder: Folder;
	};

export type RgbaColor = [number, number, number, number];
