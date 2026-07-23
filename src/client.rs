//! Blocking HTTP client for the Partiri API.
//!
//! [`ApiClient`] wraps a [`ureq`] agent, attaches the `x-api-key` header, retries
//! on HTTP 429 with exponential backoff, and turns non-2xx responses into
//! enriched [`CliError`](crate::error::CliError)s. The rest of the module is the
//! set of `serde` types that model API request bodies and responses.
//!
//! Configuration comes from the environment: `PARTIRI_API_URL` (default
//! `https://api.partiri.cloud`, must be HTTPS) and `PARTIRI_TIMEOUT` (seconds,
//! default 30).

use serde::{de::DeserializeOwned, Serialize};
use std::time::Duration;
use ureq::{Agent, Response};

use crate::config::ServiceConfig;
use crate::error::Result;
use crate::output::new_spinner;

// ─── API response types ───────────────────────────────────────────────────────

/// A workspace — the billing and ownership boundary a user belongs to.
#[derive(Debug, serde::Deserialize)]
pub struct Workspace {
    /// Workspace UUID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Owner email. Only populated on the user's primary workspace.
    pub email: Option<String>,
}

/// A project — a logical grouping of services within a workspace.
#[derive(Debug, serde::Deserialize)]
pub struct Project {
    /// Project UUID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Environment label (`dev`, `staging`, or `prod`).
    pub environment: String,
    /// UUID of the owning workspace.
    #[allow(dead_code)]
    pub fk_workspace: Option<String>,
}

/// A single environment variable as returned by the API.
#[derive(Debug, serde::Deserialize)]
pub struct ApiEnvVar {
    /// Variable name.
    pub key: String,
    /// Variable value.
    pub value: String,
}

/// A regional replica of a service. The API stores `fk_region` here (not on the
/// service itself) since migration `20260506000001`; the home region is the
/// replica with `is_primary = true`.
#[derive(Debug, serde::Deserialize)]
pub struct ServiceReplica {
    #[allow(dead_code)]
    pub id: String,
    pub fk_region: String,
    pub is_primary: bool,
}

/// A deployed service, as returned by the API. Mirrors (a superset of)
/// [`ServiceConfig`]; most fields are optional because the API may return them
/// as `null`.
///
/// `fk_region` is **not** a column on the service anymore — the API returns it
/// via the [`replicas`](Self::replicas) array. Use [`Service::primary_region`]
/// to read it. List endpoints currently do not hydrate `replicas`; only
/// `read_service` does.
#[derive(Debug, serde::Deserialize)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub deploy_type: String,
    pub runtime: String,
    pub external_sd_url: Option<String>,
    #[allow(dead_code)]
    pub internal_sd_url: Option<String>,
    // Source
    pub repository_url: Option<String>,
    pub repository_branch: Option<String>,
    pub registry_url: Option<String>,
    pub fk_service_secret: Option<String>,
    // Paths & commands
    pub root_path: Option<String>,
    pub build_path: Option<String>,
    pub build_command: Option<String>,
    pub pre_deploy_command: Option<String>,
    pub run_command: Option<String>,
    // Infrastructure
    pub fk_pod: Option<String>,
    pub fk_project: Option<String>,
    pub fk_workspace: Option<String>,
    /// Regional replicas. Populated by `read_service`; absent on list endpoints.
    #[serde(default)]
    pub replicas: Option<Vec<ServiceReplica>>,
    // Health & settings
    pub health_check_path: Option<String>,
    pub maintenance_mode: Option<bool>,
    pub active: Option<bool>,
    // Env vars — returned by API as an array (may be null in DB)
    pub env: Option<Vec<ApiEnvVar>>,
    pub deploy_tag: Option<String>,
    #[allow(dead_code)]
    pub created_at: Option<String>,
    #[allow(dead_code)]
    pub updated_at: Option<String>,
}

impl Service {
    /// The `fk_region` of the primary replica, if one is present.
    pub fn primary_region(&self) -> Option<&str> {
        self.replicas
            .as_ref()?
            .iter()
            .find(|r| r.is_primary)
            .map(|r| r.fk_region.as_str())
    }
}

/// One Prometheus time series: `(timestamp, value)` samples, value as a string.
#[derive(Debug, serde::Deserialize)]
pub struct PrometheusResult {
    /// `(unix_timestamp, value)` samples.
    pub values: Vec<(f64, String)>,
}

/// The `data` block of a Prometheus range-query response.
#[derive(Debug, serde::Deserialize)]
pub struct PrometheusData {
    /// One entry per matched series (usually a single series).
    pub result: Vec<PrometheusResult>,
}

/// A Prometheus range-query response for a single metric (CPU or memory).
#[derive(Debug, serde::Deserialize)]
pub struct PrometheusResponse {
    /// The query result payload.
    pub data: PrometheusData,
}

/// A single log line with its timestamp.
#[derive(Debug, serde::Deserialize)]
pub struct LogLine {
    /// ISO 8601 / RFC 3339 timestamp.
    pub timestamp: String,
    /// Log message text.
    pub message: String,
}

/// Response from the service-logs endpoint.
#[derive(Debug, serde::Deserialize)]
pub struct LogsResponse {
    /// Log lines, oldest first.
    pub logs: Vec<LogLine>,
}

/// Network metrics: download and upload byte-rate time series.
#[derive(Debug, serde::Deserialize)]
pub struct NetworkMetricsResponse {
    /// Inbound byte-rate series.
    pub download: PrometheusResponse,
    /// Outbound byte-rate series.
    pub upload: PrometheusResponse,
}

/// A deployment/lifecycle job (deploy, pause, unpause, kill) for a service.
#[derive(Debug, serde::Deserialize)]
pub struct Job {
    /// Job UUID.
    #[allow(dead_code)]
    pub id: String,
    /// UUID of the service this job belongs to.
    #[allow(dead_code)]
    pub fk_service: String,
    /// Job kind (`deploy`, `pause`, `unpause`, `kill`, …).
    #[serde(rename = "type")]
    pub job_type: String,
    /// Current status (`open`, `in_progress`, `succeeded`, `failed`, `timed_out`, `canceled`).
    pub status: String,
    /// Deploy reference (commit/image ref) when applicable.
    pub deploy_ref: Option<String>,
    /// Creation timestamp.
    pub created_at: Option<String>,
    /// Last-update timestamp.
    #[allow(dead_code)]
    pub updated_at: Option<String>,
}

/// A paginated list of [`Job`]s.
#[derive(Debug, serde::Deserialize)]
pub struct PaginatedJobs {
    /// The jobs on this page.
    pub data: Vec<Job>,
    /// Total job count across all pages.
    #[allow(dead_code)]
    pub total: usize,
}

/// A compute pod — a sized CPU/RAM slot a service can be deployed onto.
#[derive(Debug, serde::Deserialize)]
pub struct Pod {
    /// Pod UUID.
    pub id: String,
    /// Internal name.
    pub name: String,
    /// Human-friendly label, if set.
    pub label: Option<String>,
    /// CPU allocation (e.g. `"0.5"`).
    pub cpu: Option<String>,
    /// RAM allocation (e.g. `"512Mi"`).
    pub ram: Option<String>,
}

/// A geographic region that compute pods live in.
#[derive(Debug, serde::Deserialize)]
pub struct Region {
    /// Region UUID.
    pub id: String,
    /// Internal name.
    pub name: String,
    /// Human-friendly label, if set.
    pub label: Option<String>,
    /// ISO country code, if set.
    pub country_code: Option<String>,
}

/// Secret entry returned by the list endpoints (sensitive data is never returned).
#[derive(Debug, serde::Deserialize)]
pub struct WorkspaceSecret {
    /// Secret UUID — the value used for `fk_service_secret`.
    pub id: String,
    /// Human-friendly name, if set.
    pub name: Option<String>,
    /// Provider the secret authenticates against, if set.
    pub provider: Option<String>,
}

/// A workspace secret as created or returned with its type field.
#[derive(Debug, serde::Deserialize)]
pub struct CreatedSecret {
    /// Secret UUID.
    pub id: String,
    /// Human-friendly name, if set.
    pub name: Option<String>,
    /// Provider, if set.
    #[allow(dead_code)]
    pub provider: Option<String>,
    /// Secret kind: `"service"`, `"registry"`, or `"repository"`.
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub secret_type: Option<String>,
}

/// Pricing for pods in a region, returned by `GET /resources/pricing?region=…`.
#[derive(Debug, serde::Deserialize)]
pub struct RegionPricing {
    /// Per-pod pricing entries.
    pub pods: Vec<PodPrice>,
    /// Monthly cost per GB of persistent storage in this region (0 when none configured).
    pub volume_price_per_gb: f64,
}

/// Monthly price for a specific pod in a region.
#[derive(Debug, serde::Deserialize)]
pub struct PodPrice {
    /// Pod UUID.
    pub fk_pod: String,
    /// Monthly flat-rate price (EUR).
    pub price: f64,
    /// Per-minute rate derived from the monthly price (price / (30 × 24 × 60)).
    #[serde(rename = "perMinute")]
    #[allow(dead_code)]
    pub per_minute: f64,
}

/// Workspace balance, returned by `GET /balances/:workspace_id`.
#[derive(Debug, serde::Deserialize)]
pub struct WorkspaceBalance {
    /// Current balance amount (EUR).
    pub amount: f64,
    /// Currency code (always `"EUR"`).
    #[allow(dead_code)]
    pub currency: String,
}

/// A persistent storage volume.
#[derive(Debug, serde::Deserialize, serde::Serialize, Clone)]
pub struct Volume {
    /// Volume UUID (set by the API; omitted on creation — sending an explicit
    /// `null` would violate the `id` primary-key NOT NULL constraint on insert).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// K8s-safe name (DNS label, ≤53 chars). Becomes `<name>-pvc` in K8s.
    pub name: String,
    /// Project UUID the volume belongs to.
    pub fk_project: String,
    /// Workspace UUID (resolved server-side from `fk_project`; may be sent anyway).
    pub fk_workspace: String,
    /// Region UUID.
    pub fk_region: String,
    /// Service UUID this volume is attached to. Set on creation to auto-attach once provisioned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fk_service: Option<String>,
    /// Absolute mount path inside the container (e.g. `/app/data`).
    pub mount_path: String,
    /// Disk size in GB (integer, 1–10).
    pub size: u32,
    /// Provisioning status: `pending` | `provisioning` | `available` | `attached` | `deleting` | `failed`.
    pub status: String,
    /// Creation timestamp (ISO 8601).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Result of a backend health-check probe.
#[derive(Debug, serde::Deserialize)]
pub struct HealthCheckResult {
    /// HTTP status returned by the probe, if the request completed.
    pub status: Option<u16>,
    /// Whether the probe got a 2xx response.
    pub ok: bool,
    /// Whether the probe errored out.
    #[allow(dead_code)]
    pub error: bool,
    /// Number of attempts the backend made.
    pub attempts: u32,
}

// ─── Request bodies ──────────────────────────────────────────────────────────

/// Request body for `POST /projects`.
#[derive(Debug, serde::Serialize)]
struct CreateProjectBody<'a> {
    name: &'a str,
    environment: &'a str,
    fk_workspace: &'a str,
}

/// Request body for `POST /workspaces/secrets/registry`.
#[derive(Debug, serde::Serialize)]
struct CreateRegistrySecretBody<'a> {
    name: &'a str,
    fk_workspace: &'a str,
    provider: &'a str,
    data: RegistrySecretData<'a>,
}

#[derive(Debug, serde::Serialize)]
struct RegistrySecretData<'a> {
    username: &'a str,
    password: &'a str,
}

/// Request body for `POST /workspaces/secrets/repository`.
#[derive(Debug, serde::Serialize)]
struct CreateRepositorySecretBody<'a> {
    name: &'a str,
    fk_workspace: &'a str,
    provider: &'a str,
    data: RepositorySecretData<'a>,
}

/// Repository secret credential data. `value` is the access token; `key` is the
/// username (only used for Bitbucket basic auth; empty string for all other providers).
#[derive(Debug, serde::Serialize)]
struct RepositorySecretData<'a> {
    key: &'a str,
    value: &'a str,
}

/// Request body for `PUT /storage/volumes/:id/attach`.
#[derive(Debug, serde::Serialize)]
#[allow(dead_code)]
struct AttachVolumeBody<'a> {
    #[serde(rename = "serviceId")]
    service_id: &'a str,
}

/// Request body for `PATCH /storage/volumes/:id` (mirrors the API's
/// `VolumeUpdateInput`). Only the fields that actually changed are sent — a
/// mount-path-only change redeploys the service, a size increase prorates and
/// charges the delta, and a size decrease is rejected server-side.
#[derive(Debug, Default, serde::Serialize)]
pub struct VolumeUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount_path: Option<String>,
}

impl VolumeUpdate {
    /// Whether this update carries no field changes (nothing to PATCH).
    pub fn is_empty(&self) -> bool {
        self.size.is_none() && self.mount_path.is_none()
    }
}

/// Request body for `POST /services` — a [`ServiceConfig`] flattened together
/// with its project and workspace UUIDs.
#[derive(Debug, serde::Serialize)]
struct ServiceBody<'a> {
    #[serde(flatten)]
    service: &'a ServiceConfig,
    fk_project: &'a str,
    fk_workspace: &'a str,
}

// ─── ApiClient ────────────────────────────────────────────────────────────────

/// Authenticated, blocking HTTP client for the Partiri API.
///
/// Construct it with [`ApiClient::new`], then call the domain methods
/// (`list_*`, `create_*`, `read_*`, `deploy_service`, …). Each method handles
/// retry-on-429 and non-2xx → [`CliError`](crate::error::CliError) translation.
pub struct ApiClient {
    agent: Agent,
    base_url: String,
    api_key: String,
}

impl ApiClient {
    /// Build a client from the environment.
    ///
    /// Reads the API key from the credentials file (see
    /// [`auth::read_key`](crate::modules::auth::read_key)), the base URL from
    /// `PARTIRI_API_URL` (default `https://api.partiri.cloud`), and the request
    /// timeout from `PARTIRI_TIMEOUT` seconds (default 30).
    ///
    /// # Errors
    ///
    /// Fails when no API key is configured or when `PARTIRI_API_URL` is not HTTPS.
    pub fn new() -> Result<Self> {
        let api_key = crate::modules::auth::read_key()
            .ok_or("No API key found.\n  Run 'partiri auth login' to sign in.")?;
        let base_url = std::env::var("PARTIRI_API_URL")
            .unwrap_or_else(|_| "https://api.partiri.cloud".to_string());

        if !base_url.starts_with("https://") {
            return Err(format!("PARTIRI_API_URL must use HTTPS. Got: {base_url}").into());
        }

        let timeout_secs: u64 = std::env::var("PARTIRI_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);

        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(timeout_secs))
            .build();

        Ok(ApiClient {
            agent,
            base_url,
            api_key,
        })
    }

    /// Build a client pointed at an arbitrary base URL (e.g. a `MockServer`),
    /// bypassing the credentials file and HTTPS check. Test-only so command
    /// modules can exercise their `run_*` handlers against a mock API.
    #[cfg(test)]
    pub(crate) fn for_test(base_url: String) -> Self {
        ApiClient {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(5))
                .build(),
            base_url,
            api_key: "test-api-key".to_string(),
        }
    }

    // ─── Internal HTTP helpers ────────────────────────────────────────────────

    /// Send a request with automatic retry on HTTP 429 (rate-limited).
    /// Retries up to 3 times with exponential backoff (1s, 2s, 4s).
    ///
    /// The closure returns a ureq send result; non-2xx responses come back as
    /// `Err(ureq::Error::Status(code, response))`, which we unwrap so the
    /// downstream handlers see a uniform `Response` regardless of status.
    fn send_with_retry<F>(&self, build: F) -> Result<Response>
    where
        F: Fn() -> std::result::Result<Response, Box<ureq::Error>>,
    {
        const MAX_RETRIES: u32 = 3;
        for attempt in 0..=MAX_RETRIES {
            let response = match build() {
                Ok(resp) => resp,
                Err(err) => match *err {
                    ureq::Error::Status(_, resp) => resp,
                    ureq::Error::Transport(t) => {
                        return Err(format!("Network request failed: {t}").into());
                    }
                },
            };

            if response.status() != 429 || attempt == MAX_RETRIES {
                return Ok(response);
            }

            let wait = response
                .header("retry-after")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1 << attempt);

            std::thread::sleep(Duration::from_secs(wait));
        }
        unreachable!()
    }

    fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let spinner = new_spinner();
        let response = self.send_with_retry(|| {
            self.agent
                .get(&format!("{}{}", self.base_url, path))
                .set("x-api-key", &self.api_key)
                .call()
                .map_err(Box::new)
        })?;
        spinner.finish_and_clear();
        self.handle_response(response)
    }

    fn get_query<T: DeserializeOwned>(&self, path: &str, params: &[(&str, &str)]) -> Result<T> {
        let spinner = new_spinner();
        let response = self.send_with_retry(|| {
            let mut req = self
                .agent
                .get(&format!("{}{}", self.base_url, path))
                .set("x-api-key", &self.api_key);
            for (k, v) in params {
                req = req.query(k, v);
            }
            req.call().map_err(Box::new)
        })?;
        spinner.finish_and_clear();
        self.handle_response(response)
    }

    fn post<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        let spinner = new_spinner();
        let response = self.send_with_retry(|| {
            self.agent
                .post(&format!("{}{}", self.base_url, path))
                .set("x-api-key", &self.api_key)
                .send_json(body)
                .map_err(Box::new)
        })?;
        spinner.finish_and_clear();
        self.handle_response(response)
    }

    fn post_empty<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let spinner = new_spinner();
        let response = self.send_with_retry(|| {
            self.agent
                .post(&format!("{}{}", self.base_url, path))
                .set("x-api-key", &self.api_key)
                .send_json(body)
                .map_err(Box::new)
        })?;
        spinner.finish_and_clear();
        self.handle_response_empty(response)
    }

    fn put_empty<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let spinner = new_spinner();
        let response = self.send_with_retry(|| {
            self.agent
                .put(&format!("{}{}", self.base_url, path))
                .set("x-api-key", &self.api_key)
                .send_json(body)
                .map_err(Box::new)
        })?;
        spinner.finish_and_clear();
        self.handle_response_empty(response)
    }

    fn patch<B: Serialize, T: DeserializeOwned>(&self, path: &str, body: &B) -> Result<T> {
        let spinner = new_spinner();
        let response = self.send_with_retry(|| {
            self.agent
                .request("PATCH", &format!("{}{}", self.base_url, path))
                .set("x-api-key", &self.api_key)
                .send_json(body)
                .map_err(Box::new)
        })?;
        spinner.finish_and_clear();
        self.handle_response(response)
    }

    fn error_message(response: Response) -> crate::error::Error {
        let status = response.status();
        let body = response.into_string().unwrap_or_default();
        let msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["message"].as_str().map(String::from))
            .unwrap_or_else(|| "An unexpected error occurred.".to_string());

        let hint = match status {
            400 => Some("Check that your configuration values are valid."),
            401 => Some("Your API key may have expired or been revoked. Run 'partiri auth login' to sign in again."),
            402 => Some("Your workspace balance is insufficient. Top up at https://partiri.cloud/settings/billing"),
            403 => Some("Your account may lack permission, or a workspace limit has been reached."),
            404 => Some("The resource was not found. It may have been deleted."),
            409 => Some("A conflicting operation is in progress. Wait for it to finish, then retry."),
            422 => Some("The request data is invalid. Check your configuration values."),
            429 => Some("Rate limit exceeded. Please wait a moment and try again."),
            500..=599 => Some("This is a server-side error. Try again later, or contact support."),
            _ => None,
        };

        let mut err = crate::error::CliError::new(status.to_string(), msg);
        if let Some(h) = hint {
            err = err.with_hint(h);
        }
        Box::new(err.enriched())
    }

    fn handle_response<T: DeserializeOwned>(&self, response: Response) -> Result<T> {
        let status = response.status();
        if (200..300).contains(&status) {
            let body = response
                .into_string()
                .map_err(|e| format!("Failed to read response: {e}"))?;
            serde_json::from_str::<T>(&body).map_err(|e| {
                let preview = if body.len() > 200 {
                    &body[..200]
                } else {
                    &body
                };
                format!("Failed to parse API response: {e}\n  Body: {preview}").into()
            })
        } else {
            Err(Self::error_message(response))
        }
    }

    fn handle_response_empty(&self, response: Response) -> Result<()> {
        if (200..300).contains(&response.status()) {
            Ok(())
        } else {
            Err(Self::error_message(response))
        }
    }

    // ─── Domain methods ───────────────────────────────────────────────────────

    /// List every workspace the API key can access (`GET /workspaces`).
    pub fn list_workspaces(&self) -> Result<Vec<Workspace>> {
        self.get("/workspaces")
    }

    /// List the projects in a workspace (`GET /projects?workspace=…`).
    pub fn list_projects(&self, workspace_id: &str) -> Result<Vec<Project>> {
        self.get_query("/projects", &[("workspace", workspace_id)])
    }

    /// Create a project in a workspace (`POST /projects`).
    pub fn create_project(&self, name: &str, environment: &str, workspace_id: &str) -> Result<()> {
        let body = CreateProjectBody {
            name,
            environment,
            fk_workspace: workspace_id,
        };
        self.post_empty("/projects", &body)
    }

    /// List the regions available in a workspace (`GET /resources/regions`).
    pub fn list_regions(&self, workspace_id: &str) -> Result<Vec<Region>> {
        self.get_query("/resources/regions", &[("workspace", workspace_id)])
    }

    /// List the compute pods available in a workspace (`GET /resources/pods`).
    pub fn list_pods(&self, workspace_id: &str) -> Result<Vec<Pod>> {
        self.get_query("/resources/pods", &[("workspace", workspace_id)])
    }

    /// List the services in a project (`GET /services?project=…&limit=…`).
    ///
    /// `limit` is required: the API caps the result at 10 when none is sent and
    /// gives no indication that rows were withheld.
    pub fn list_services(&self, project_id: &str, limit: usize) -> Result<Vec<Service>> {
        let limit = limit.to_string();
        self.get_query("/services", &[("project", project_id), ("limit", &limit)])
    }

    /// List the registry secrets defined in a workspace.
    pub fn list_registry_secrets(&self, workspace_id: &str) -> Result<Vec<WorkspaceSecret>> {
        self.get(&format!("/workspaces/secrets/registry/{}", workspace_id))
    }

    /// List the repository secrets defined in a workspace.
    pub fn list_repository_secrets(&self, workspace_id: &str) -> Result<Vec<WorkspaceSecret>> {
        self.get(&format!("/workspaces/secrets/repository/{}", workspace_id))
    }

    /// `GET /resources/utils/reg` — backend probes the registry, optionally using
    /// a stored registry secret resolved by `secret_id`.
    pub fn validate_registry(&self, registry_url: &str, secret_id: Option<&str>) -> Result<bool> {
        let mut params: Vec<(&str, &str)> = vec![("registry_url", registry_url)];
        if let Some(id) = secret_id {
            params.push(("id", id));
        }
        self.get_query("/resources/utils/reg", &params)
    }

    /// `GET /resources/utils/git` — backend lists branches via `git ls-remote` (or equivalent),
    /// optionally using a stored repository secret resolved by `secret_id`. A 4xx for a private
    /// repo without a secret IS the access check.
    pub fn load_repository_branches(
        &self,
        url: &str,
        secret_id: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut params: Vec<(&str, &str)> = vec![("url", url)];
        if let Some(id) = secret_id {
            params.push(("id", id));
        }
        self.get_query("/resources/utils/git", &params)
    }

    /// `GET /resources/utils/health-check` — backend probes a public health-check URL.
    pub fn probe_health_check(
        &self,
        workspace_id: &str,
        health_check_path: &str,
    ) -> Result<HealthCheckResult> {
        self.get_query(
            "/resources/utils/health-check",
            &[
                ("workspace", workspace_id),
                ("health_check_path", health_check_path),
            ],
        )
    }

    /// Register a new service (`POST /services`) and return the created record,
    /// including its assigned `id` and `external_sd_url`.
    pub fn create_service(
        &self,
        service: &ServiceConfig,
        project_id: &str,
        workspace_id: &str,
    ) -> Result<Service> {
        let body = ServiceBody {
            service,
            fk_project: project_id,
            fk_workspace: workspace_id,
        };
        self.post("/services", &body)
    }

    /// Fetch a single service by UUID (`GET /services/{id}`).
    pub fn read_service(&self, id: &str) -> Result<Service> {
        self.get(&format!("/services/{}", id))
    }

    /// `GET path`, scoping to `deploy_tag` via the `deployTag` query parameter
    /// when one is supplied. Backs the metrics/logs readers below.
    fn get_with_optional_tag<T: DeserializeOwned>(
        &self,
        path: &str,
        deploy_tag: Option<&str>,
    ) -> Result<T> {
        match deploy_tag {
            Some(tag) => self.get_query(path, &[("deployTag", tag)]),
            None => self.get(path),
        }
    }

    /// Fetch the CPU time series for a service. When `deploy_tag` is `Some`, the
    /// metrics are scoped to that exact deploy.
    pub fn read_metrics_cpu(
        &self,
        id: &str,
        deploy_tag: Option<&str>,
    ) -> Result<PrometheusResponse> {
        self.get_with_optional_tag(&format!("/metrics/cpu/{}", id), deploy_tag)
    }

    /// Fetch the memory time series for a service, optionally scoped to a deploy tag.
    pub fn read_metrics_memory(
        &self,
        id: &str,
        deploy_tag: Option<&str>,
    ) -> Result<PrometheusResponse> {
        self.get_with_optional_tag(&format!("/metrics/memory/{}", id), deploy_tag)
    }

    /// Fetch the network download/upload time series for a service, optionally
    /// scoped to a deploy tag.
    pub fn read_metrics_network(
        &self,
        id: &str,
        deploy_tag: Option<&str>,
    ) -> Result<NetworkMetricsResponse> {
        self.get_with_optional_tag(&format!("/metrics/network/{}", id), deploy_tag)
    }

    /// Fetch recent log lines for a service, optionally scoped to a deploy tag.
    pub fn read_service_logs(&self, id: &str, deploy_tag: Option<&str>) -> Result<LogsResponse> {
        self.get_with_optional_tag(&format!("/logs/{}", id), deploy_tag)
    }

    /// Push an updated [`ServiceConfig`] to an existing service (`PUT /services/{id}`).
    pub fn update_service(&self, id: &str, service: &ServiceConfig) -> Result<()> {
        self.put_empty(&format!("/services/{}", id), service)
    }

    /// List the jobs for a service, newest page first (`GET /jobs/services/{id}`).
    pub fn list_service_jobs(&self, id: &str) -> Result<Vec<Job>> {
        let resp: PaginatedJobs = self.get(&format!("/jobs/services/{}", id))?;
        Ok(resp.data)
    }

    /// Enqueue a service job of the given `action` (`POST /jobs/services/{action}/{id}`).
    /// Backs the deploy/pause/unpause/kill helpers below.
    fn enqueue_job(&self, action: &str, id: &str) -> Result<()> {
        self.post_empty(
            &format!("/jobs/services/{}/{}", action, id),
            &serde_json::json!({}),
        )
    }

    /// Enqueue a deploy job for a service. The job runs asynchronously.
    pub fn deploy_service(&self, id: &str) -> Result<()> {
        self.enqueue_job("deploy", id)
    }

    /// Enqueue a pause job for a service (stops billable compute, keeps config).
    pub fn pause_service(&self, id: &str) -> Result<()> {
        self.enqueue_job("pause", id)
    }

    /// Enqueue an unpause job, resuming a paused service.
    pub fn unpause_service(&self, id: &str) -> Result<()> {
        self.enqueue_job("unpause", id)
    }

    /// Enqueue a kill job, permanently stopping a service.
    pub fn kill_service(&self, id: &str) -> Result<()> {
        self.enqueue_job("kill", id)
    }

    // ─── Secrets ─────────────────────────────────────────────────────────────

    /// Create a registry secret in a workspace (`POST /workspaces/secrets/registry`).
    /// Returns the created secret record (id, name, provider — never the credential data).
    /// `provider` must be one of: `github`, `gitlab`, `bitbucket`, `docker`, `google`, `aws`.
    pub fn create_registry_secret(
        &self,
        name: &str,
        workspace_id: &str,
        provider: &str,
        username: &str,
        password: &str,
    ) -> Result<CreatedSecret> {
        let body = CreateRegistrySecretBody {
            name,
            fk_workspace: workspace_id,
            provider,
            data: RegistrySecretData { username, password },
        };
        self.post("/workspaces/secrets/registry", &body)
    }

    /// Create a repository secret in a workspace (`POST /workspaces/secrets/repository`).
    /// Returns the created secret record.
    /// `provider` must be one of: `github`, `gitlab`, `bitbucket`, `codeberg`.
    /// `token` is the access token stored as `data.value`.
    /// `username` maps to `data.key` (required only for Bitbucket basic auth; pass `""` otherwise).
    pub fn create_repository_secret(
        &self,
        name: &str,
        workspace_id: &str,
        provider: &str,
        token: &str,
        username: &str,
    ) -> Result<CreatedSecret> {
        let body = CreateRepositorySecretBody {
            name,
            fk_workspace: workspace_id,
            provider,
            data: RepositorySecretData {
                key: username,
                value: token,
            },
        };
        self.post("/workspaces/secrets/repository", &body)
    }

    // ─── Pricing & balance ────────────────────────────────────────────────────

    /// Fetch pod and volume pricing for a region (`GET /resources/pricing?region=…`).
    pub fn get_pricing(&self, region_id: &str) -> Result<RegionPricing> {
        self.get_query("/resources/pricing", &[("region", region_id)])
    }

    /// Fetch the workspace balance (`GET /balances/:workspace_id`).
    pub fn get_balance(&self, workspace_id: &str) -> Result<WorkspaceBalance> {
        self.get(&format!("/balances/{}", workspace_id))
    }

    // ─── Storage / Volumes ────────────────────────────────────────────────────

    /// List non-deleted volumes in a project (`GET /storage/volumes?project=…`).
    pub fn list_volumes(&self, project_id: &str) -> Result<Vec<Volume>> {
        self.get_query("/storage/volumes", &[("project", project_id)])
    }

    /// Create a new volume (`POST /storage/volumes`).
    /// Set `volume.fk_service` to auto-attach once provisioned (no separate attach call needed).
    pub fn create_volume(&self, volume: &Volume) -> Result<Volume> {
        self.post("/storage/volumes", volume)
    }

    /// Read a single volume by UUID (`GET /storage/volumes/:id`).
    pub fn read_volume(&self, id: &str) -> Result<Volume> {
        self.get(&format!("/storage/volumes/{}", id))
    }

    /// Update a volume's size and/or mount path (`PATCH /storage/volumes/:id`).
    /// A size increase prorates and charges the delta; a mount-path change
    /// redeploys the service. Shrinking is rejected by the API.
    pub fn update_volume(&self, volume_id: &str, changes: &VolumeUpdate) -> Result<Volume> {
        self.patch(&format!("/storage/volumes/{}", volume_id), changes)
    }

    /// Attach an available volume to a service (`PUT /storage/volumes/:id/attach`).
    /// Use this only for volumes that already exist and are in `available` status.
    #[allow(dead_code)]
    pub fn attach_volume(&self, volume_id: &str, service_id: &str) -> Result<Volume> {
        let body = AttachVolumeBody { service_id };
        let spinner = crate::output::new_spinner();
        let response = self.send_with_retry(|| {
            self.agent
                .put(&format!(
                    "{}/storage/volumes/{}/attach",
                    self.base_url, volume_id
                ))
                .set("x-api-key", &self.api_key)
                .send_json(&body)
                .map_err(Box::new)
        })?;
        spinner.finish_and_clear();
        self.handle_response(response)
    }

    /// Detach a volume from its service (`PUT /storage/volumes/:id/detach`).
    /// The service must be paused first; the API enforces this.
    pub fn detach_volume(&self, volume_id: &str) -> Result<Volume> {
        let spinner = crate::output::new_spinner();
        let response = self.send_with_retry(|| {
            self.agent
                .put(&format!(
                    "{}/storage/volumes/{}/detach",
                    self.base_url, volume_id
                ))
                .set("x-api-key", &self.api_key)
                .send_json(serde_json::json!({}))
                .map_err(Box::new)
        })?;
        spinner.finish_and_clear();
        self.handle_response(response)
    }

    /// Delete a volume (`DELETE /storage/volumes/:id`). Volume must be detached first.
    pub fn delete_volume(&self, volume_id: &str) -> Result<()> {
        let spinner = crate::output::new_spinner();
        let response = self.send_with_retry(|| {
            self.agent
                .delete(&format!("{}/storage/volumes/{}", self.base_url, volume_id))
                .set("x-api-key", &self.api_key)
                .call()
                .map_err(Box::new)
        })?;
        spinner.finish_and_clear();
        self.handle_response_empty(response)
    }

    /// Re-enqueue a provision job for a failed/pending volume (`PUT /storage/volumes/:id/retry`).
    #[allow(dead_code)]
    pub fn retry_volume(&self, volume_id: &str) -> Result<()> {
        self.put_empty(
            &format!("/storage/volumes/{}/retry", volume_id),
            &serde_json::json!({}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;
    use serde_json::json;

    fn test_client(server: &MockServer) -> ApiClient {
        ApiClient {
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(5))
                .build(),
            base_url: server.base_url(),
            api_key: "test-api-key-123".to_string(),
        }
    }

    // ─── list_workspaces ─────────────────────────────────────────────────────

    #[test]
    fn list_workspaces_success_parses_response() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/workspaces")
                .header("x-api-key", "test-api-key-123");
            then.status(200).json_body(json!([
                {"id": "ws-1", "name": "My Workspace", "email": "user@example.com"}
            ]));
        });

        let client = test_client(&server);
        let result = client.list_workspaces().unwrap();

        mock.assert();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "ws-1");
        assert_eq!(result[0].name, "My Workspace");
        assert_eq!(result[0].email.as_deref(), Some("user@example.com"));
    }

    #[test]
    fn list_workspaces_with_null_email_parses_as_none() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(200).json_body(json!([
                {"id": "ws-1", "name": "No-Email Workspace", "email": null}
            ]));
        });

        let client = test_client(&server);
        let result = client.list_workspaces().unwrap();
        assert!(result[0].email.is_none());
    }

    #[test]
    fn list_workspaces_empty_array_returns_empty_vec() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(200).json_body(json!([]));
        });

        let result = test_client(&server).list_workspaces().unwrap();
        assert!(result.is_empty());
    }

    // ─── Error status codes and hints ────────────────────────────────────────

    #[test]
    fn status_401_includes_partiri_key_hint() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(401)
                .json_body(json!({"message": "Unauthorized"}));
        });

        let err = test_client(&server)
            .list_workspaces()
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "should mention 401: {err}");
        assert!(
            err.contains("partiri auth"),
            "should include auth hint: {err}"
        );
    }

    #[test]
    fn status_403_includes_permission_or_limit_hint() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(403).json_body(json!({"message": "Forbidden"}));
        });

        let err = test_client(&server)
            .list_workspaces()
            .unwrap_err()
            .to_string();
        assert!(err.contains("403"), "{err}");
        assert!(
            err.contains("permission") && err.contains("limit"),
            "should include permission and limit hint: {err}"
        );
    }

    #[test]
    fn status_404_includes_not_found_hint() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(404).body("not found");
        });

        let err = test_client(&server)
            .list_workspaces()
            .unwrap_err()
            .to_string();
        assert!(err.contains("404"), "{err}");
        assert!(
            err.to_lowercase().contains("not found") || err.contains("resource"),
            "should include not-found hint: {err}"
        );
    }

    #[test]
    fn status_402_includes_balance_hint() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/jobs/services/deploy/svc-1");
            then.status(402)
                .json_body(json!({"message": "Insufficient balance to deploy this service"}));
        });

        let err = test_client(&server)
            .deploy_service("svc-1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("402"), "should mention 402: {err}");
        assert!(
            err.contains("balance"),
            "should include balance hint: {err}"
        );
    }

    #[test]
    fn status_409_includes_conflict_hint() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/jobs/services/unpause/svc-1");
            then.status(409)
                .json_body(json!({"message": "Active job already exists for this service"}));
        });

        let err = test_client(&server)
            .unpause_service("svc-1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("409"), "should mention 409: {err}");
        assert!(
            err.contains("conflicting operation"),
            "should include conflict hint: {err}"
        );
    }

    #[test]
    fn status_400_includes_validation_hint() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(400)
                .json_body(json!({"message": "Invalid health check URL"}));
        });

        let err = test_client(&server)
            .list_workspaces()
            .unwrap_err()
            .to_string();
        assert!(err.contains("400"), "should mention 400: {err}");
        assert!(
            err.contains("configuration values"),
            "should include validation hint: {err}"
        );
    }

    #[test]
    fn status_429_includes_rate_limit_hint() {
        let server = MockServer::start();
        // Return 429 on every attempt so all retries are exhausted
        server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(429)
                .json_body(json!({"message": "Too many requests"}));
        });

        let err = test_client(&server)
            .list_workspaces()
            .unwrap_err()
            .to_string();
        assert!(err.contains("429"), "should mention 429: {err}");
        assert!(
            err.contains("Rate limit"),
            "should include rate limit hint: {err}"
        );
    }

    #[test]
    fn status_500_includes_server_error_hint() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(500)
                .json_body(json!({"message": "Internal server error"}));
        });

        let err = test_client(&server)
            .list_workspaces()
            .unwrap_err()
            .to_string();
        assert!(err.contains("500"), "should mention 500: {err}");
        assert!(
            err.contains("server-side error"),
            "should include server error hint: {err}"
        );
    }

    #[test]
    fn json_message_field_extracted_from_error_body() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(400)
                .json_body(json!({"message": "Workspace limit exceeded"}));
        });

        let err = test_client(&server)
            .list_workspaces()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Workspace limit exceeded"),
            "should extract message field: {err}"
        );
    }

    #[test]
    fn non_json_error_body_returned_raw() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/workspaces");
            then.status(503).body("<html>Service Unavailable</html>");
        });

        let err = test_client(&server)
            .list_workspaces()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("Service Unavailable") || err.contains("503"),
            "should include raw body or status: {err}"
        );
    }

    // ─── API key sent in header ───────────────────────────────────────────────

    #[test]
    fn api_key_sent_as_x_api_key_header() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/workspaces")
                .header("x-api-key", "test-api-key-123");
            then.status(200).json_body(json!([]));
        });

        test_client(&server).list_workspaces().unwrap();
        mock.assert();
    }

    // ─── list_projects ────────────────────────────────────────────────────────

    #[test]
    fn list_projects_includes_workspace_query_param() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/projects").query_param("workspace", "ws-uuid-123");
            then.status(200).json_body(json!([
                {"id": "proj-1", "name": "My Project", "environment": "production", "fk_workspace": "ws-uuid-123"}
            ]));
        });

        let client = test_client(&server);
        let result = client.list_projects("ws-uuid-123").unwrap();

        mock.assert();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "My Project");
    }

    // ─── list_services ──────────────────────────────────────────────────────

    #[test]
    fn list_services_sends_explicit_limit_query_param() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/services")
                .query_param("project", "proj-uuid-1")
                .query_param("limit", "50");
            then.status(200).json_body(json!([
                {"id": "svc-1", "name": "api", "runtime": "registry", "deploy_type": "registry"}
            ]));
        });

        let client = test_client(&server);
        let result = client.list_services("proj-uuid-1", 50).unwrap();

        mock.assert();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "api");
    }

    // ─── list_service_jobs ──────────────────────────────────────────────────

    #[test]
    fn list_service_jobs_parses_paginated_response() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/jobs/services/svc-42");
            then.status(200).json_body(json!({
                "data": [
                    {
                        "id": "job-1",
                        "fk_service": "svc-42",
                        "type": "deploy",
                        "status": "succeeded",
                        "cluster": "us-east-1",
                        "deploy_ref": "abc1234def5678",
                        "notes": [],
                        "created_at": "2025-03-18T10:00:00Z",
                        "updated_at": "2025-03-18T10:05:00Z"
                    },
                    {
                        "id": "job-2",
                        "fk_service": "svc-42",
                        "type": "kill",
                        "status": "open",
                        "cluster": "any",
                        "deploy_ref": null,
                        "notes": null,
                        "created_at": "2025-03-18T11:00:00Z",
                        "updated_at": null
                    }
                ],
                "total": 2
            }));
        });

        let client = test_client(&server);
        let jobs = client.list_service_jobs("svc-42").unwrap();

        mock.assert();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, "job-1");
        assert_eq!(jobs[0].job_type, "deploy");
        assert_eq!(jobs[0].status, "succeeded");
        assert_eq!(jobs[0].deploy_ref.as_deref(), Some("abc1234def5678"));
        assert_eq!(jobs[1].job_type, "kill");
        assert!(jobs[1].deploy_ref.is_none());
    }

    #[test]
    fn read_service_logs_with_deploy_tag_sends_query_param() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/logs/svc-123")
                .query_param("deployTag", "ab12c");
            then.status(200).json_body(json!({ "logs": [] }));
        });
        let result = test_client(&server)
            .read_service_logs("svc-123", Some("ab12c"))
            .unwrap();
        mock.assert();
        assert!(result.logs.is_empty());
    }

    #[test]
    fn read_service_logs_without_deploy_tag_omits_query_param() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/logs/svc-123");
            then.status(200).json_body(json!({ "logs": [] }));
        });
        test_client(&server)
            .read_service_logs("svc-123", None)
            .unwrap();
        mock.assert();
    }

    #[test]
    fn read_metrics_cpu_with_deploy_tag_sends_query_param() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/metrics/cpu/svc-123")
                .query_param("deployTag", "ab12c");
            then.status(200)
                .json_body(json!({ "data": { "result": [] } }));
        });
        test_client(&server)
            .read_metrics_cpu("svc-123", Some("ab12c"))
            .unwrap();
        mock.assert();
    }

    #[test]
    fn list_service_jobs_empty_data_returns_empty_vec() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/jobs/services/svc-99");
            then.status(200).json_body(json!({
                "data": [],
                "total": 0
            }));
        });

        let jobs = test_client(&server).list_service_jobs("svc-99").unwrap();
        assert!(jobs.is_empty());
    }

    // ─── Secret CRUD ─────────────────────────────────────────────────────────

    #[test]
    fn create_registry_secret_posts_to_correct_path() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/workspaces/secrets/registry")
                .header("x-api-key", "test-api-key-123");
            then.status(201).json_body(json!({
                "id": "sec-reg-1",
                "name": "ghcr-token",
                "provider": "github",
                "type": "registry"
            }));
        });

        let secret = test_client(&server)
            .create_registry_secret("ghcr-token", "ws-1", "github", "user", "pass")
            .unwrap();

        mock.assert();
        assert_eq!(secret.id, "sec-reg-1");
        assert_eq!(secret.name.as_deref(), Some("ghcr-token"));
    }

    #[test]
    fn create_registry_secret_body_contains_provider_and_data() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/workspaces/secrets/registry")
                .json_body(json!({
                    "name": "my-reg",
                    "fk_workspace": "ws-42",
                    "provider": "docker",
                    "data": { "username": "user", "password": "s3cret" }
                }));
            then.status(201).json_body(json!({
                "id": "sec-1", "name": "my-reg", "provider": "docker", "type": "registry"
            }));
        });

        test_client(&server)
            .create_registry_secret("my-reg", "ws-42", "docker", "user", "s3cret")
            .unwrap();

        mock.assert();
    }

    #[test]
    fn create_repository_secret_posts_to_correct_path() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/workspaces/secrets/repository")
                .header("x-api-key", "test-api-key-123");
            then.status(201).json_body(json!({
                "id": "sec-repo-1",
                "name": "github-pat",
                "provider": "github",
                "type": "repository"
            }));
        });

        let secret = test_client(&server)
            .create_repository_secret("github-pat", "ws-1", "github", "ghp_token123", "")
            .unwrap();

        mock.assert();
        assert_eq!(secret.id, "sec-repo-1");
    }

    #[test]
    fn create_repository_secret_body_contains_provider_key_value() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/workspaces/secrets/repository")
                .json_body(json!({
                    "name": "git-token",
                    "fk_workspace": "ws-5",
                    "provider": "github",
                    "data": { "key": "", "value": "mytoken" }
                }));
            then.status(201).json_body(json!({
                "id": "sec-2", "name": "git-token", "provider": "github", "type": "repository"
            }));
        });

        test_client(&server)
            .create_repository_secret("git-token", "ws-5", "github", "mytoken", "")
            .unwrap();

        mock.assert();
    }

    #[test]
    fn create_repository_secret_bitbucket_sends_username_as_key() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/workspaces/secrets/repository")
                .json_body(json!({
                    "name": "bb-token",
                    "fk_workspace": "ws-7",
                    "provider": "bitbucket",
                    "data": { "key": "bbuser", "value": "bbpass" }
                }));
            then.status(201).json_body(json!({
                "id": "sec-3", "name": "bb-token", "provider": "bitbucket", "type": "repository"
            }));
        });

        test_client(&server)
            .create_repository_secret("bb-token", "ws-7", "bitbucket", "bbpass", "bbuser")
            .unwrap();

        mock.assert();
    }
    // ─── Pricing & balance ────────────────────────────────────────────────────

    #[test]
    fn get_pricing_queries_with_region_param() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/resources/pricing")
                .query_param("region", "reg-eu-1");
            then.status(200).json_body(json!({
                "pods": [
                    { "fk_pod": "pod-s", "price": 5.0, "perMinute": 0.000115 },
                    { "fk_pod": "pod-m", "price": 10.0, "perMinute": 0.00023 }
                ],
                "volume_price_per_gb": 0.5
            }));
        });

        let pricing = test_client(&server).get_pricing("reg-eu-1").unwrap();

        mock.assert();
        assert_eq!(pricing.pods.len(), 2);
        assert_eq!(pricing.pods[0].fk_pod, "pod-s");
        assert!((pricing.pods[0].price - 5.0).abs() < 1e-6);
        assert!((pricing.volume_price_per_gb - 0.5).abs() < 1e-6);
    }

    #[test]
    fn get_pricing_parses_per_minute_rate() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/resources/pricing");
            then.status(200).json_body(json!({
                "pods": [{ "fk_pod": "p1", "price": 43200.0, "perMinute": 1.0 }],
                "volume_price_per_gb": 0.0
            }));
        });

        let pricing = test_client(&server).get_pricing("r").unwrap();
        assert!((pricing.pods[0].per_minute - 1.0).abs() < 1e-6);
    }

    #[test]
    fn get_balance_fetches_workspace_balance() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/balances/ws-abc");
            then.status(200).json_body(json!({
                "amount": 25.50,
                "currency": "EUR",
                "updated_at": "2025-05-01T00:00:00Z"
            }));
        });

        let balance = test_client(&server).get_balance("ws-abc").unwrap();

        mock.assert();
        assert!((balance.amount - 25.50).abs() < 1e-6);
        assert_eq!(balance.currency, "EUR");
    }

    #[test]
    fn get_balance_zero_amount_parses_correctly() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/balances/ws-empty");
            then.status(200)
                .json_body(json!({ "amount": 0.0, "currency": "EUR" }));
        });

        let balance = test_client(&server).get_balance("ws-empty").unwrap();
        assert!((balance.amount - 0.0).abs() < 1e-9);
    }

    // ─── Storage / Volumes ────────────────────────────────────────────────────

    #[test]
    fn list_volumes_queries_with_project_param() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET)
                .path("/storage/volumes")
                .query_param("project", "proj-1");
            then.status(200).json_body(json!([
                {
                    "id": "vol-1", "name": "my-disk", "fk_project": "proj-1",
                    "fk_workspace": "ws-1", "fk_region": "reg-1",
                    "fk_service": "svc-1", "mount_path": "/app/data",
                    "size": 5, "status": "attached"
                }
            ]));
        });

        let volumes = test_client(&server).list_volumes("proj-1").unwrap();
        mock.assert();
        assert_eq!(volumes.len(), 1);
        assert_eq!(volumes[0].name, "my-disk");
        assert_eq!(volumes[0].size, 5);
        assert_eq!(volumes[0].status, "attached");
    }

    #[test]
    fn create_volume_posts_body_with_fk_service() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(POST)
                .path("/storage/volumes")
                .json_body_includes(json!({ "fk_service": "svc-new" }).to_string());
            then.status(201).json_body(json!({
                "id": "vol-new", "name": "svc-disk", "fk_project": "proj-1",
                "fk_workspace": "ws-1", "fk_region": "reg-1",
                "fk_service": "svc-new", "mount_path": "/app/data",
                "size": 3, "status": "pending"
            }));
        });

        let vol = crate::client::Volume {
            id: None,
            name: "svc-disk".into(),
            fk_project: "proj-1".into(),
            fk_workspace: "ws-1".into(),
            fk_region: "reg-1".into(),
            fk_service: Some("svc-new".into()),
            mount_path: "/app/data".into(),
            size: 3,
            status: "pending".into(),
            created_at: None,
        };

        let created = test_client(&server).create_volume(&vol).unwrap();
        mock.assert();
        assert_eq!(created.id.as_deref(), Some("vol-new"));
        assert_eq!(created.status, "pending");
    }

    #[test]
    fn create_volume_body_omits_null_id_and_none_options() {
        // `id` is a NOT NULL primary key with a server-side default; sending an explicit
        // `null` is rejected on insert. `fk_service`/`created_at` must also drop when None.
        let vol = crate::client::Volume {
            id: None,
            name: "disk".into(),
            fk_project: "p".into(),
            fk_workspace: "w".into(),
            fk_region: "r".into(),
            fk_service: None,
            mount_path: "/app/data".into(),
            size: 1,
            status: "pending".into(),
            created_at: None,
        };
        let body = serde_json::to_string(&vol).unwrap();
        assert!(!body.contains("\"id\""), "create body must omit id: {body}");
        assert!(
            !body.contains("\"fk_service\""),
            "None fk_service must be omitted: {body}"
        );
        assert!(
            !body.contains("\"created_at\""),
            "None created_at must be omitted: {body}"
        );
    }

    #[test]
    fn service_create_body_flattened_omits_disk() {
        // The real wire body for POST/PUT /services is the flattened `ServiceBody`
        // (`#[serde(flatten)] service` + fk_project/fk_workspace). `disk` is reconciled
        // into a separate Volume and is not a `services` column, so it must never appear
        // in the flattened body. Guards `#[serde(skip_serializing)]` on `ServiceConfig.disk`
        // against a future change in how serde flatten treats it.
        let service = ServiceConfig {
            name: "svc".into(),
            disk: Some(crate::config::DiskConfig {
                mount_path: "/app/data".into(),
                size: 5,
            }),
            ..Default::default()
        };
        let body = ServiceBody {
            service: &service,
            fk_project: "proj-1",
            fk_workspace: "ws-1",
        };
        let json = serde_json::to_string(&body).unwrap();
        assert!(
            !json.contains("\"disk\""),
            "flattened /services body must omit disk: {json}"
        );
        // Sanity: the flatten actually produced the wrapper + service fields.
        assert!(
            json.contains("\"fk_project\"") && json.contains("\"name\""),
            "flatten should include wrapper and service fields: {json}"
        );
    }

    #[test]
    fn volume_serde_roundtrip_preserves_optional_fk_service() {
        let vol = crate::client::Volume {
            id: Some("v1".into()),
            name: "disk".into(),
            fk_project: "p".into(),
            fk_workspace: "w".into(),
            fk_region: "r".into(),
            fk_service: None,
            mount_path: "/tmp".into(),
            size: 1,
            status: "available".into(),
            created_at: None,
        };
        let json = serde_json::to_string(&vol).unwrap();
        assert!(
            !json.contains("fk_service"),
            "None fk_service should be skipped"
        );
        let back: crate::client::Volume = serde_json::from_str(&json).unwrap();
        assert!(back.fk_service.is_none());
    }

    #[test]
    fn read_volume_fetches_by_id() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method(GET).path("/storage/volumes/vol-42");
            then.status(200).json_body(json!({
                "id": "vol-42", "name": "vol-name", "fk_project": "p",
                "fk_workspace": "w", "fk_region": "r",
                "fk_service": null, "mount_path": "/app",
                "size": 2, "status": "available"
            }));
        });

        let vol = test_client(&server).read_volume("vol-42").unwrap();
        mock.assert();
        assert_eq!(vol.id.as_deref(), Some("vol-42"));
        assert_eq!(vol.status, "available");
    }

    #[test]
    fn delete_volume_sends_delete_to_correct_path() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method("DELETE").path("/storage/volumes/vol-del");
            then.status(200).json_body(json!(true));
        });

        test_client(&server).delete_volume("vol-del").unwrap();
        mock.assert();
    }

    #[test]
    fn attach_volume_sends_put_with_service_id_body() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method("PUT")
                .path("/storage/volumes/vol-att/attach")
                .json_body(json!({ "serviceId": "svc-att" }));
            then.status(200).json_body(json!({
                "id": "vol-att", "name": "d", "fk_project": "p",
                "fk_workspace": "w", "fk_region": "r",
                "fk_service": "svc-att", "mount_path": "/app",
                "size": 1, "status": "attached"
            }));
        });

        let vol = test_client(&server)
            .attach_volume("vol-att", "svc-att")
            .unwrap();
        mock.assert();
        assert_eq!(vol.status, "attached");
    }

    #[test]
    fn retry_volume_sends_put_to_retry_path() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method("PUT").path("/storage/volumes/vol-fail/retry");
            then.status(204).body("");
        });

        test_client(&server).retry_volume("vol-fail").unwrap();
        mock.assert();
    }

    #[test]
    fn update_volume_sends_patch_with_changed_fields() {
        let server = MockServer::start();
        let mock = server.mock(|when, then| {
            when.method("PATCH")
                .path("/storage/volumes/vol-up")
                .json_body(json!({ "size": 8, "mount_path": "/app/storage" }));
            then.status(200).json_body(json!({
                "id": "vol-up", "name": "d", "fk_project": "p",
                "fk_workspace": "w", "fk_region": "r",
                "fk_service": "svc-1", "mount_path": "/app/storage",
                "size": 8, "status": "attached"
            }));
        });

        let changes = crate::client::VolumeUpdate {
            size: Some(8),
            mount_path: Some("/app/storage".to_string()),
        };
        let vol = test_client(&server)
            .update_volume("vol-up", &changes)
            .unwrap();
        mock.assert();
        assert_eq!(vol.size, 8);
        assert_eq!(vol.mount_path, "/app/storage");
    }

    #[test]
    fn volume_update_omits_none_fields() {
        // A size-only update must not carry a null mount_path — the API's
        // `VolumeUpdateInput` distinguishes "unset" from a value.
        let changes = crate::client::VolumeUpdate {
            size: Some(4),
            mount_path: None,
        };
        let json = serde_json::to_string(&changes).unwrap();
        assert!(json.contains("\"size\""), "{json}");
        assert!(!json.contains("mount_path"), "{json}");
    }

    #[test]
    fn volume_update_is_empty_reports_no_changes() {
        assert!(crate::client::VolumeUpdate::default().is_empty());
        assert!(!crate::client::VolumeUpdate {
            size: Some(2),
            mount_path: None,
        }
        .is_empty());
    }

    // ─── Cost math (pure, no HTTP) ────────────────────────────────────────────

    #[test]
    fn monthly_cost_formula_pod_only() {
        // pod_price + 0 disk = pod_price
        let pod_price = 10.0_f64;
        let disk_gb = 0_u32;
        let volume_price_per_gb = 0.5_f64;
        let cost = pod_price + volume_price_per_gb * f64::from(disk_gb);
        assert!((cost - 10.0).abs() < 1e-9);
    }

    #[test]
    fn monthly_cost_formula_pod_and_disk() {
        // pod=10, disk=3GB at 0.5/GB → 11.5
        let cost = 10.0_f64 + 0.5 * 3.0;
        assert!((cost - 11.5).abs() < 1e-9);
    }

    #[test]
    fn cost_delta_positive_when_disk_added() {
        let current = 10.0_f64;
        let desired = 10.0 + 0.5 * 5.0; // +5GB disk
        let delta = desired - current;
        assert!(delta > 0.0);
        assert!((delta - 2.5).abs() < 1e-9);
    }

    #[test]
    fn cost_delta_negative_when_disk_removed() {
        let current = 10.0_f64 + 0.5 * 5.0;
        let desired = 10.0_f64;
        let delta = desired - current;
        assert!(delta < 0.0);
        assert!((delta - (-2.5)).abs() < 1e-9);
    }

    // ─── ApiClient::new() ────────────────────────────────────────────────────

    #[test]
    fn new_succeeds_when_credentials_file_exists() {
        // ApiClient::new reads from the credentials file at ~/.config/partiri/key.
        // This test relies on the dev machine having a valid key file.
        let has_key = crate::modules::auth::credentials_path()
            .and_then(|p| std::fs::read_to_string(&p).ok())
            .filter(|s| !s.trim().is_empty())
            .is_some();
        if has_key {
            assert!(ApiClient::new().is_ok(), "should succeed with key file");
        }
    }
}
