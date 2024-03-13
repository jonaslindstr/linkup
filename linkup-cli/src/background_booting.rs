use std::thread;
use std::time::{Duration, Instant};

use reqwest::StatusCode;

use linkup::{StorableService, StorableSession, UpdateSessionRequest};
use url::Url;

use crate::local_config::{LocalState, ServiceTarget};
use crate::services::local_server::{is_local_server_started, start_local_server};
use crate::services::tunnel::{is_tunnel_started, start_tunnel};
use crate::status::print_session_names;
use crate::worker_client::WorkerClient;
use crate::{linkup_file_path, services, LINKUP_LOCALSERVER_PORT};
use crate::{CliError, LINKUP_LOCALDNS_INSTALL};

pub fn boot_background_services() -> Result<(), CliError> {
    let mut state = LocalState::load()?;

    let local_url = Url::parse(&format!("http://localhost:{}", LINKUP_LOCALSERVER_PORT))
        .expect("linkup url invalid");

    if is_local_server_started().is_err() {
        println!("Starting linkup local server...");
        start_local_server()?;
    } else {
        println!("Linkup local server was already running.. Try stopping linkup first if you have problems.");
    }

    wait_till_ok(format!("{}linkup-check", local_url))?;

    if state.should_use_tunnel() {
        if is_tunnel_started().is_err() {
            println!("Starting tunnel...");
            let tunnel = start_tunnel()?;
            state.linkup.tunnel = Some(tunnel);
        } else {
            println!("Cloudflare tunnel was already running.. Try stopping linkup first if you have problems.");
        }
    } else {
        println!(
            "Skipping tunnel start... WARNING: not all kinds of requests will work in this mode."
        );
    }

    let server_config = ServerConfig::from(&state);

    let server_session_name = load_config(
        &state.linkup.remote,
        &state.linkup.session_name,
        server_config.remote,
    )?;
    let local_session_name = load_config(&local_url, &server_session_name, server_config.local)?;

    if server_session_name != local_session_name {
        return Err(CliError::InconsistentState);
    }

    state.linkup.session_name = server_session_name;
    state.save()?;

    if linkup_file_path(LINKUP_LOCALDNS_INSTALL).exists() {
        boot_local_dns(state.domain_strings(), state.linkup.session_name.clone())?;
    }

    if let Some(tunnel) = &state.linkup.tunnel {
        println!("Waiting for tunnel to be ready at {}...", tunnel);

        // If the tunnel is checked too quickly, it dies ¯\_(ツ)_/¯
        thread::sleep(Duration::from_millis(1000));
        wait_till_ok(format!("{}linkup-check", tunnel))?;

        println!();
    }

    print_session_names(&state);

    Ok(())
}

pub fn load_config(
    url: &Url,
    desired_name: &str,
    config: StorableSession,
) -> Result<String, CliError> {
    let session_update_req = UpdateSessionRequest {
        session_token: config.session_token,
        desired_name: desired_name.to_string(),
        services: config.services,
        domains: config.domains,
        cache_routes: config.cache_routes,
    };

    let content = WorkerClient::new(url)
        .linkup(&session_update_req)
        .map_err(|e| CliError::LoadConfig(url.to_string(), e.to_string()))?;

    Ok(content)
}

pub fn boot_local_dns(domains: Vec<String>, session_name: String) -> Result<(), CliError> {
    services::caddy::start(domains.clone())?;
    services::dnsmasq::start(domains, session_name)?;

    Ok(())
}

pub struct ServerConfig {
    pub local: StorableSession,
    pub remote: StorableSession,
}

impl From<&LocalState> for ServerConfig {
    fn from(state: &LocalState) -> Self {
        let local_server_services = state
            .services
            .iter()
            .map(|service| StorableService {
                name: service.name.clone(),
                location: if service.current == ServiceTarget::Remote {
                    service.remote.clone()
                } else {
                    service.local.clone()
                },
                rewrites: Some(service.rewrites.clone()),
            })
            .collect::<Vec<StorableService>>();

        let remote_server_services = state
            .services
            .iter()
            .map(|service| StorableService {
                name: service.name.clone(),
                location: if service.current == ServiceTarget::Remote {
                    service.remote.clone()
                } else {
                    state.get_tunnel_url()
                },
                rewrites: Some(service.rewrites.clone()),
            })
            .collect::<Vec<StorableService>>();

        let local_storable_session = StorableSession {
            session_token: state.linkup.session_token.clone(),
            services: local_server_services,
            domains: state.domains.clone(),
            cache_routes: state.linkup.cache_routes.clone(),
        };

        let remote_storable_session = StorableSession {
            session_token: state.linkup.session_token.clone(),
            services: remote_server_services,
            domains: state.domains.clone(),
            cache_routes: state.linkup.cache_routes.clone(),
        };

        ServerConfig {
            local: local_storable_session,
            remote: remote_storable_session,
        }
    }
}

impl<'a> From<&'a ServerConfig> for (&'a StorableSession, &'a StorableSession) {
    fn from(config: &'a ServerConfig) -> Self {
        (&config.local, &config.remote)
    }
}

pub fn wait_till_ok(url: String) -> Result<(), CliError> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .map_err(|err| CliError::StartLinkupTimeout(err.to_string()))?;

    let start = Instant::now();
    loop {
        if start.elapsed() > Duration::from_secs(20) {
            return Err(CliError::StartLinkupTimeout(format!(
                "{} took too long to load",
                url
            )));
        }

        let response = client.get(&url).send();

        if let Ok(resp) = response {
            if resp.status() == StatusCode::OK {
                return Ok(());
            }
        }

        thread::sleep(Duration::from_millis(2000));
    }
}
