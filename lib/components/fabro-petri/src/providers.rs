//! Built-in sandbox providers shared by Petri execution and server access.
//!
//! Configuration is captured by the caller, with Daytona credentials from
//! the vault. Factories connect lazily per run; a Host-only run needs neither
//! Docker nor Daytona. Host registry ownership stays with Petri: server
//! attach uses an observer instead of these factories.
//!
//! Every Petri runtime Fabro builds starts from [`standard_runtime`] or
//! [`bare_runtime`], so none falls back to launching a provider plugin,
//! which a release build cannot verify.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use fabro_static::EnvVars;
use petri_runtime::{
    InProcessProviders, ProviderContext, ProviderFactory, ProviderNetwork, Runtime, fingerprint,
};
use sandbox_driver::{AuthError, Error, ProviderKind, SandboxProvider};
use sandbox_driver_daytona::{DaytonaConfig, DaytonaProvider};
use sandbox_driver_docker::DockerProvider;
use sandbox_driver_host::HostProvider;

const USER_AGENT: &str = concat!("fabro-server/", env!("CARGO_PKG_VERSION"));

/// Explicit Daytona credentials: the SDK's configuration with the API key
/// always present and a `Debug` that never prints it. The process
/// environment is never consulted.
#[derive(Clone)]
pub struct DaytonaCredentials(DaytonaConfig);

impl DaytonaCredentials {
    /// Credentials for `api_key` against Daytona's public control plane,
    /// presenting Fabro's `User-Agent`.
    #[must_use]
    pub fn new(api_key: String) -> Self {
        Self(DaytonaConfig {
            api_key: Some(api_key),
            user_agent: Some(USER_AGENT.to_string()),
            ..DaytonaConfig::default()
        })
    }

    /// Credentials for a vault API key, with the control-plane URL and
    /// organization taken from `lookup` (server configuration). Nothing is
    /// read implicitly.
    ///
    /// These are the two settings the Daytona plugin read in the worker, so
    /// a lease it recorded keeps its fingerprint: no URL alias and no
    /// placement target, neither of which reached the plugin.
    pub fn from_api_key(api_key: String, lookup: impl Fn(&str) -> Option<String>) -> Self {
        Self::new(api_key)
            .with_api_url(lookup(EnvVars::DAYTONA_API_URL))
            .with_organization_id(lookup(EnvVars::DAYTONA_ORGANIZATION_ID))
    }

    /// The control-plane URL; Daytona's public API when `None`.
    #[must_use]
    pub fn with_api_url(mut self, api_url: Option<String>) -> Self {
        self.0.api_url = api_url;
        self
    }

    #[must_use]
    pub fn with_organization_id(mut self, organization_id: Option<String>) -> Self {
        self.0.organization_id = organization_id;
        self
    }

    /// A shared HTTP client; tests pass a no-proxy client here.
    #[must_use]
    pub fn with_http_client(mut self, http_client: Option<fabro_http::HttpClient>) -> Self {
        self.0.http_client = http_client;
        self
    }

    /// The SDK configuration the driver's Daytona provider connects with.
    #[must_use]
    fn config(&self) -> &DaytonaConfig {
        &self.0
    }
}

impl std::fmt::Debug for DaytonaCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaytonaCredentials")
            .field("api_url", &self.0.api_url)
            .field("organization_id", &self.0.organization_id)
            .finish_non_exhaustive()
    }
}

/// The provider configuration supplied to a run or prune. Defaults are
/// local Docker selection and no Daytona credentials; constructing this
/// configuration never connects to a backend or requires a credential.
#[derive(Clone, Debug, Default)]
pub struct SandboxProviderConfig {
    docker_host:         Option<String>,
    docker_host_address: Option<String>,
    docker_host_network: bool,
    daytona:             Option<DaytonaCredentials>,
}

impl SandboxProviderConfig {
    /// The configuration for `daytona`'s credentials, with how a remote
    /// Docker daemon's containers reach this machine from `lookup`.
    ///
    /// The Docker endpoint is read from this process's `DOCKER_HOST`, never
    /// from `lookup`: the Docker client connects to the daemon that
    /// variable names, so the lease fingerprint and network name the daemon
    /// the sandboxes are actually created on.
    pub fn from_lookup(
        daytona: Option<DaytonaCredentials>,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Self {
        #[expect(
            clippy::disallowed_methods,
            reason = "the Docker client reads DOCKER_HOST from this process; the fingerprint must name the same daemon"
        )]
        let docker_host = std::env::var(EnvVars::DOCKER_HOST).ok();
        Self {
            docker_host,
            docker_host_address: lookup(EnvVars::PETRI_SANDBOX_DOCKER_HOST_ADDRESS)
                .filter(|value| !value.trim().is_empty()),
            docker_host_network: false,
            daytona,
        }
    }

    /// Use the Docker daemon host's network namespace for Docker sandboxes.
    #[must_use]
    pub fn with_docker_host_network(mut self, enabled: bool) -> Self {
        self.docker_host_network = enabled;
        self
    }

    /// Whether Docker sandboxes use the Docker daemon host's network
    /// namespace.
    #[must_use]
    pub fn docker_host_network(&self) -> bool {
        self.docker_host_network
    }
}

/// Petri's standard runtime with Fabro's built-in providers installed.
#[must_use]
pub fn standard_runtime(config: &SandboxProviderConfig) -> Runtime {
    #[expect(
        clippy::disallowed_methods,
        reason = "the one place a standard runtime is built, with the built-in providers installed"
    )]
    let runtime = Runtime::standard();
    runtime.in_process_providers(built_in_providers(config))
}

/// Petri's bare runtime with Fabro's built-in providers installed.
#[must_use]
pub fn bare_runtime(config: &SandboxProviderConfig) -> Runtime {
    #[expect(
        clippy::disallowed_methods,
        reason = "the one place a bare runtime is built, with the built-in providers installed"
    )]
    let runtime = Runtime::bare();
    runtime.in_process_providers(built_in_providers(config))
}

/// The Docker connection used by both the server and Petri. The driver reads
/// the caller process's Docker endpoint/TLS environment; health is checked
/// by the caller so diagnostics can report an unavailable daemon.
#[expect(
    clippy::result_large_err,
    reason = "preserve sandbox-driver's typed error, as required by Petri's ProviderFactory contract"
)]
pub fn connect_docker() -> sandbox_driver::Result<Arc<dyn SandboxProvider>> {
    Ok(Arc::new(DockerProvider::connect_unverified()?))
}

/// Connect using only the vault credentials and explicit control-plane
/// configuration; no ambient secret fallback is permitted.
pub async fn connect_daytona(
    credentials: &DaytonaCredentials,
) -> sandbox_driver::Result<Arc<dyn SandboxProvider>> {
    Ok(Arc::new(
        DaytonaProvider::connect_explicit(credentials.config().clone()).await?,
    ))
}

/// One lazy factory per built-in kind. Missing Daytona credentials fail
/// only when a Daytona scope is acquired, never for admission or a Host run.
fn built_in_providers(config: &SandboxProviderConfig) -> InProcessProviders {
    InProcessProviders::new()
        .with(Arc::new(HostFactory))
        .with(Arc::new(DockerFactory {
            host:         config.docker_host.clone(),
            host_address: config.docker_host_address.clone(),
            host_network: config.docker_host_network,
        }))
        .with(Arc::new(DaytonaFactory(config.daytona.clone())))
}

struct HostFactory;

#[async_trait]
impl ProviderFactory for HostFactory {
    fn kind(&self) -> &'static str {
        "host"
    }

    /// An empty registry path when Petri supplies none, as the plugin
    /// recorded it.
    fn fingerprint_seed(&self, context: &ProviderContext) -> String {
        fingerprint::host(context.host_registry().unwrap_or(Path::new("")))
    }

    fn network(&self) -> ProviderNetwork {
        ProviderNetwork::host()
    }

    async fn connect(
        &self,
        context: &ProviderContext,
    ) -> sandbox_driver::Result<Arc<dyn SandboxProvider>> {
        let registry = context.host_registry().ok_or_else(|| {
            Error::invalid_spec(
                "host_registry",
                "Petri supplied no Host registry for this run",
            )
        })?;
        Ok(Arc::new(HostProvider::with_registry(registry).await?))
    }
}

struct DockerFactory {
    host:         Option<String>,
    host_address: Option<String>,
    host_network: bool,
}

impl DockerFactory {
    fn seed(&self) -> String {
        fingerprint::docker(self.host.as_deref())
    }

    fn provider_network(&self) -> ProviderNetwork {
        let host = self.host.as_deref();
        let host_address = if self.host_network && docker_host_is_local(host) {
            self.host_address.as_deref().or(Some("127.0.0.1"))
        } else {
            self.host_address.as_deref()
        };
        ProviderNetwork::docker(host, host_address)
    }
}

#[async_trait]
impl ProviderFactory for DockerFactory {
    fn kind(&self) -> &'static str {
        "docker"
    }

    fn fingerprint_seed(&self, _context: &ProviderContext) -> String {
        self.seed()
    }

    fn network(&self) -> ProviderNetwork {
        self.provider_network()
    }

    async fn connect(
        &self,
        _context: &ProviderContext,
    ) -> sandbox_driver::Result<Arc<dyn SandboxProvider>> {
        connect_docker()
    }
}

/// Matches Petri's Docker endpoint classification. Host networking changes
/// the route from a local sandbox to this process, but it does not make a
/// remote Docker daemon local or make its host filesystem mountable.
fn docker_host_is_local(docker_host: Option<&str>) -> bool {
    let value = docker_host.unwrap_or_default().trim();
    value.is_empty() || value.starts_with("unix://") || value.starts_with("npipe://")
}

struct DaytonaFactory(Option<DaytonaCredentials>);

impl DaytonaFactory {
    fn seed(&self) -> String {
        let config = self.0.as_ref().map(DaytonaCredentials::config);
        fingerprint::daytona(
            config.and_then(|config| config.api_url.as_deref()),
            config.and_then(|config| config.organization_id.as_deref()),
            None,
        )
    }
}

#[async_trait]
impl ProviderFactory for DaytonaFactory {
    fn kind(&self) -> &'static str {
        "daytona"
    }

    fn fingerprint_seed(&self, _context: &ProviderContext) -> String {
        self.seed()
    }

    fn network(&self) -> ProviderNetwork {
        ProviderNetwork::none()
    }

    async fn connect(
        &self,
        _context: &ProviderContext,
    ) -> sandbox_driver::Result<Arc<dyn SandboxProvider>> {
        let credentials = self.0.as_ref().ok_or_else(|| Error::Auth(AuthError::new(
            ProviderKind::try_new("daytona").expect("the built-in provider kind is valid"),
            "Daytona requires DAYTONA_API_KEY in the vault; run `fabro secret set DAYTONA_API_KEY`",
        )))?;
        connect_daytona(credentials).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn docker_fingerprint_keeps_the_plugin_namespace_for_unset_and_configured_hosts() {
        for (host, expected) in [
            (None, "docker:default"),
            (Some("  "), "docker:default"),
            (Some(" tcp://daemon:2376 "), "docker:tcp://daemon:2376"),
            (
                Some("unix:///var/run/docker.sock"),
                "docker:unix:///var/run/docker.sock",
            ),
        ] {
            let factory = DockerFactory {
                host:         host.map(str::to_owned),
                host_address: None,
                host_network: false,
            };
            assert_eq!(factory.seed(), expected);
        }
    }

    #[test]
    fn daytona_fingerprint_keeps_the_plugin_seed() {
        let unset = DaytonaCredentials::from_api_key("test-key".to_string(), |_| None);
        assert_eq!(DaytonaFactory(Some(unset)).seed(), "daytona:::");
        let configured =
            DaytonaCredentials::from_api_key("test-key".to_string(), |name| match name {
                EnvVars::DAYTONA_API_URL => Some("https://daytona.example".to_string()),
                EnvVars::DAYTONA_SERVER_URL => Some("https://ignored.example".to_string()),
                EnvVars::DAYTONA_ORGANIZATION_ID => Some("org-1".to_string()),
                EnvVars::DAYTONA_TARGET => Some("us".to_string()),
                _ => None,
            });
        let factory = DaytonaFactory(Some(configured));
        assert_eq!(factory.seed(), "daytona:https://daytona.example:org-1:");
        assert_eq!(factory.region(), None);
    }

    #[test]
    fn daytona_configuration_ignores_the_url_alias_and_keeps_the_http_client() {
        let credentials = DaytonaCredentials::from_api_key("test-key".to_string(), |name| {
            (name == EnvVars::DAYTONA_SERVER_URL).then(|| "https://alias.example".to_string())
        })
        .with_http_client(Some(fabro_test::test_http_client()));
        assert_eq!(credentials.config().api_url, None);
        assert!(credentials.config().http_client.is_some());
    }

    #[test]
    fn provider_configuration_keeps_the_key_but_never_prints_it() {
        let key = "dtn_test_sensitive_value";
        let config = SandboxProviderConfig::from_lookup(
            Some(DaytonaCredentials::from_api_key(key.to_string(), |name| {
                (name == EnvVars::DAYTONA_ORGANIZATION_ID).then(|| "org-1".to_string())
            })),
            |_| None,
        );
        let daytona = config.daytona.as_ref().expect("the credentials are kept");
        assert_eq!(daytona.config().api_key.as_deref(), Some(key));
        let rendered = format!("{config:?}");
        assert!(!rendered.contains(key));
        assert!(rendered.contains("org-1"));
    }

    #[test]
    fn provider_configuration_defaults_host_network_off() {
        let default = SandboxProviderConfig::from_lookup(None, |_| None);
        assert!(!default.docker_host_network());
    }

    #[test]
    fn bridge_docker_uses_the_gateway_and_keeps_local_workspace_mounts() {
        let factory = DockerFactory {
            host:         None,
            host_address: None,
            host_network: false,
        };
        assert_eq!(
            format!("{:?}", factory.provider_network()),
            "ProviderNetwork { host_address: Ok(Some(\"host.docker.internal\")), \
             supports_host_workspace: true }"
        );
    }

    #[test]
    fn local_host_network_uses_loopback_and_keeps_local_workspace_mounts() {
        let factory = DockerFactory {
            host:         Some("unix:///var/run/docker.sock".to_string()),
            host_address: None,
            host_network: true,
        };
        assert_eq!(
            format!("{:?}", factory.provider_network()),
            "ProviderNetwork { host_address: Ok(Some(\"127.0.0.1\")), \
             supports_host_workspace: true }"
        );
    }

    #[test]
    fn local_host_network_preserves_an_explicit_callback_address() {
        let factory = DockerFactory {
            host:         None,
            host_address: Some("fabro.internal".to_string()),
            host_network: true,
        };
        assert_eq!(
            format!("{:?}", factory.provider_network()),
            "ProviderNetwork { host_address: Ok(Some(\"fabro.internal\")), \
             supports_host_workspace: true }"
        );
    }

    #[test]
    fn remote_host_network_keeps_the_explicit_callback_address_and_disables_mounts() {
        let factory = DockerFactory {
            host:         Some("tcp://docker.example:2376".to_string()),
            host_address: Some("fabro.internal".to_string()),
            host_network: true,
        };
        assert_eq!(
            format!("{:?}", factory.provider_network()),
            "ProviderNetwork { host_address: Ok(Some(\"fabro.internal\")), \
             supports_host_workspace: false }"
        );
    }

    #[test]
    fn remote_host_network_still_requires_a_callback_address() {
        let factory = DockerFactory {
            host:         Some("ssh://builder@docker.example".to_string()),
            host_address: None,
            host_network: true,
        };
        let network = format!("{:?}", factory.provider_network());
        assert!(network.contains("Err("), "{network}");
        assert!(
            network.contains("ssh://builder@docker.example"),
            "{network}"
        );
        assert!(
            network.contains("supports_host_workspace: false"),
            "{network}"
        );
    }
}
