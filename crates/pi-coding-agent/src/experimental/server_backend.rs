//! The server's Session catalog, plugin selections, and routing host (the backend half of upstream
//! `startServer` and `startServerBackend`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use pi_chord::node::FacetBundleArtifact;
use pi_chord::{ChordError, ChordResult, Context, Json, RemoteServiceErrorCode};
use pi_server::{RoutedServerServiceHost, RoutedSessionHandle, ServerError, ServerErrorCode, ServerHost};

use super::plugins::bundled::create_presentation_facet_data;
use super::plugins::package::{
    ConfiguredServerPluginPackage, create_server_plugin_package, normalize_plugin_package_paths,
    read_session_plugin_package_profile, remove_session_plugin_package_profile, write_session_plugin_package_profile,
};
use super::process::lock;
use super::services::server::{PreparedSessionPlugins, ServerServicesBackend};
use super::services::sessions::{SessionCreateOptions, SessionSummary};
use super::session_catalog::{SessionCatalogMetadata, create_session, delete_session, list_sessions, read_session};
use super::session_worker_manager::{SessionPluginSelectionConflictError, SessionWorkerManager};

#[derive(Clone, Default)]
pub struct ResolvedSessionPlugins {
    pub package_paths: Vec<String>,
    pub manifest_paths: Vec<String>,
    pub presentation_artifacts: Vec<FacetBundleArtifact>,
}

/// Server-owned plugin builds and the per-Session plugin selections.
pub struct PluginSelections {
    directory: PathBuf,
    server_id: String,
    packages: Mutex<HashMap<String, ConfiguredServerPluginPackage>>,
    default_selection: Mutex<ResolvedSessionPlugins>,
    sessions: Mutex<HashMap<String, ResolvedSessionPlugins>>,
    pub(super) workers: OnceLock<SessionWorkerManager>,
}

impl PluginSelections {
    pub async fn new(directory: &Path, server_id: &str, package_paths: &[String]) -> ChordResult<Arc<Self>> {
        let selections = Arc::new(Self {
            directory: directory.to_path_buf(),
            server_id: server_id.to_string(),
            packages: Mutex::default(),
            default_selection: Mutex::default(),
            sessions: Mutex::default(),
            workers: OnceLock::new(),
        });
        *lock(&selections.default_selection) = selections.build(package_paths).await?;
        Ok(selections)
    }

    fn package(&self, package_path: &str) -> ConfiguredServerPluginPackage {
        let mut packages = lock(&self.packages);
        let package = packages.entry(package_path.to_string());
        package.or_insert_with(|| create_server_plugin_package(&self.directory, &self.server_id, package_path)).clone()
    }

    async fn build(&self, package_paths: &[String]) -> ChordResult<ResolvedSessionPlugins> {
        let package_paths = normalize_plugin_package_paths(package_paths)?;
        let plugins: Vec<ConfiguredServerPluginPackage> = package_paths.iter().map(|path| self.package(path)).collect();
        let built = futures::future::try_join_all(plugins.iter().map(ConfiguredServerPluginPackage::build)).await?;
        Ok(ResolvedSessionPlugins {
            package_paths,
            manifest_paths: plugins.iter().map(|plugin| plugin.manifest_path.display().to_string()).collect(),
            presentation_artifacts: built.into_iter().flatten().collect(),
        })
    }

    fn assert_manifests(&self, metadata: &SessionCatalogMetadata, manifest_paths: &[String]) -> ChordResult<()> {
        match self.workers.get() {
            Some(workers) => workers.assert_session_plugin_manifest_paths(metadata, manifest_paths),
            None => Ok(()),
        }
    }

    pub async fn resolve(
        &self,
        metadata: &SessionCatalogMetadata,
        requested: Option<Vec<String>>,
    ) -> ChordResult<ResolvedSessionPlugins> {
        let (directory, server_id) = (&self.directory, &self.server_id);
        if let Some(requested) = requested {
            let normalized = normalize_plugin_package_paths(&requested)?;
            let current = lock(&self.sessions).get(&metadata.path).cloned();
            if let Some(current) = current.filter(|current| current.package_paths == normalized) {
                return Ok(current);
            }
            let manifests: Vec<String> =
                normalized.iter().map(|path| self.package(path).manifest_path.display().to_string()).collect();
            self.assert_manifests(metadata, &manifests)?;
            let candidate = self.build(&normalized).await?;
            self.assert_manifests(metadata, &candidate.manifest_paths)?;
            write_session_plugin_package_profile(directory, server_id, &metadata.path, &candidate.package_paths)
                .await?;
            lock(&self.sessions).insert(metadata.path.clone(), candidate.clone());
            return Ok(candidate);
        }
        if let Some(cached) = lock(&self.sessions).get(&metadata.path).cloned() {
            return Ok(cached);
        }
        let stored = read_session_plugin_package_profile(directory, server_id, &metadata.path).await?;
        let selected = match &stored {
            Some(stored) => self.build(stored).await?,
            None => lock(&self.default_selection).clone(),
        };
        if stored.is_none() {
            write_session_plugin_package_profile(directory, server_id, &metadata.path, &selected.package_paths).await?;
        }
        lock(&self.sessions).insert(metadata.path.clone(), selected.clone());
        Ok(selected)
    }

    pub async fn remove(&self, metadata: &SessionCatalogMetadata) -> ChordResult<()> {
        lock(&self.sessions).remove(&metadata.path);
        remove_session_plugin_package_profile(&self.directory, &self.server_id, &metadata.path).await
    }

    pub async fn reload(&self, package_paths: &[String]) -> ChordResult<Vec<FacetBundleArtifact>> {
        let reloaded = self.build(package_paths).await?;
        {
            let mut default = lock(&self.default_selection);
            if default.package_paths == reloaded.package_paths {
                *default = reloaded.clone();
            }
        }
        for selected in lock(&self.sessions).values_mut() {
            if selected.package_paths == reloaded.package_paths {
                *selected = reloaded.clone();
            }
        }
        Ok(reloaded.presentation_artifacts)
    }
}

/// The Session catalog and routing host one server generation serves.
pub struct ExperimentalServerHost {
    pub server_id: String,
    pub session_dir: PathBuf,
    pub workers: SessionWorkerManager,
    pub plugins: Arc<PluginSelections>,
    pub services: OnceLock<Arc<dyn RoutedServerServiceHost>>,
}

impl ExperimentalServerHost {
    async fn list(&self) -> ChordResult<Vec<SessionCatalogMetadata>> {
        let mut sessions = list_sessions(&self.session_dir).await?;
        for tracked in self.workers.tracked_sessions() {
            match sessions.iter_mut().find(|metadata| metadata.path == tracked.path) {
                Some(existing) => *existing = tracked,
                None => sessions.push(tracked),
            }
        }
        Ok(sessions)
    }

    async fn resolve(&self, session_id: &str) -> ChordResult<SessionCatalogMetadata> {
        let tracked = self.workers.tracked_sessions().into_iter().find(|metadata| metadata.id == session_id);
        let metadata = match tracked {
            Some(metadata) => Some(metadata),
            None => read_session(&self.session_dir, session_id).await,
        };
        metadata.ok_or_else(|| ServerError::session_not_found(format!("Unknown session: {session_id}")).into())
    }

    fn summarize(&self, metadata: &SessionCatalogMetadata) -> SessionSummary {
        SessionSummary {
            server_id: self.server_id.clone(),
            session_id: metadata.id.clone(),
            created_at: metadata.created_at,
        }
    }
}

#[async_trait]
impl ServerServicesBackend for ExperimentalServerHost {
    async fn list(&self, _context: &Context) -> ChordResult<Vec<SessionSummary>> {
        let mut sessions: Vec<SessionSummary> =
            self.list().await?.iter().map(|metadata| self.summarize(metadata)).collect();
        sessions.sort_by(|left, right| {
            left.session_id.cmp(&right.session_id).then(left.created_at.total_cmp(&right.created_at))
        });
        Ok(sessions)
    }

    async fn create(&self, options: SessionCreateOptions, _context: &Context) -> ChordResult<SessionSummary> {
        let cwd = std::env::current_dir().map_err(ChordError::other)?.display().to_string();
        Ok(self.summarize(&create_session(&self.session_dir, options.id.as_deref(), &cwd).await?))
    }

    async fn remove(&self, session_id: &str, context: &Context) -> ChordResult<()> {
        let metadata = self.resolve(session_id).await?;
        self.workers.close_session(&metadata, context).await?;
        delete_session(&metadata).await?;
        self.plugins.remove(&metadata).await
    }

    async fn prepare_session_plugins(
        &self,
        session_id: &str,
        package_paths: Option<Vec<String>>,
        _context: &Context,
    ) -> ChordResult<PreparedSessionPlugins> {
        let metadata = self.resolve(session_id).await?;
        let selected = self.plugins.resolve(&metadata, package_paths).await;
        let selected = selected.and_then(|selected| {
            self.workers.assert_session_plugin_manifest_paths(&metadata, &selected.manifest_paths)?;
            Ok(selected)
        });
        let selected = selected.map_err(|error| match &error {
            ChordError::Other(inner) if inner.downcast_ref::<SessionPluginSelectionConflictError>().is_some() => {
                let code = ServerErrorCode::Remote(RemoteServiceErrorCode::ServiceInvalidValue);
                ServerError::new(code, error.to_string()).into()
            }
            _ => error,
        })?;
        let presentation_plugins = create_presentation_facet_data(&selected.presentation_artifacts);
        Ok(PreparedSessionPlugins { package_paths: selected.package_paths, presentation_plugins })
    }

    async fn reload_presentation_plugins(&self, package_paths: Vec<String>, _context: &Context) -> ChordResult<Json> {
        Ok(create_presentation_facet_data(&self.plugins.reload(&package_paths).await?))
    }
}

/// The routing host; the server services refer back to the host through [`ServerServicesBackend`].
pub struct RoutingHost(pub Arc<ExperimentalServerHost>);

#[async_trait]
impl ServerHost for RoutingHost {
    type Metadata = SessionCatalogMetadata;

    fn server_services(&self) -> Arc<dyn RoutedServerServiceHost> {
        self.0.services.get().cloned().unwrap_or_else(|| unreachable!("server services are installed before start"))
    }

    async fn resolve_session(&self, session_id: &str, _context: &Context) -> ChordResult<SessionCatalogMetadata> {
        self.0.resolve(session_id).await
    }

    async fn open_session(
        &self,
        metadata: SessionCatalogMetadata,
        context: &Context,
    ) -> ChordResult<Arc<dyn RoutedSessionHandle>> {
        let selected = self.0.plugins.resolve(&metadata, None).await?;
        self.0.workers.open_session(&metadata, context, &selected.manifest_paths).await
    }
}
