use std::{
    collections::BTreeMap,
    env, io,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
    sync::Arc,
};

use multicore_core::{
    CoreLogBuffer, DeviceIdentity, MihomoRuntimeControl, PersistentSnapshotStore,
    ReqwestHttpClient, RuntimePaths, SidecarProcessController, Snapshot, SubscriptionFetcher,
    default_ipv4_interface, stage_runtime_with_mihomo_control, xray_outbound_server_domains,
};
use multicore_daemon::{
    BackendError, CoreBackend, MihomoHttpSelector, PreparedController, bind_loopback,
    publish_readiness_if_enabled, transactional_controller_factory, try_router,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = required_env("MULTICORE_DAEMON_TOKEN")?;
    let address: SocketAddr = env::var("MULTICORE_DAEMON_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8787".to_owned())
        .parse()?;
    let data_directory = PathBuf::from(required_env("MULTICORE_DAEMON_DATA_DIR")?);
    let device_identity = DeviceIdentity::load_or_create(&data_directory)?;
    let xray_binary = PathBuf::from(required_env("MULTICORE_XRAY_BIN")?);
    let mihomo_binary = PathBuf::from(required_env("MULTICORE_MIHOMO_BIN")?);
    let mihomo_controller_address: SocketAddr = env::var("MULTICORE_MIHOMO_CONTROLLER_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:19090".to_owned())
        .parse()?;
    if !mihomo_controller_address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MULTICORE_MIHOMO_CONTROLLER_ADDR must be an IP loopback SocketAddr",
        )
        .into());
    }
    let mut secret_bytes = [0_u8; 32];
    getrandom::fill(&mut secret_bytes)?;
    let mihomo_controller_secret: String = secret_bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let runtime_control =
        MihomoRuntimeControl::new(mihomo_controller_address, mihomo_controller_secret.clone())?;
    let selector = Arc::new(MihomoHttpSelector::new(
        mihomo_controller_address,
        &mihomo_controller_secret,
    )?);
    drop(mihomo_controller_secret);

    let store = PersistentSnapshotStore::open(data_directory.join("snapshots"))?;
    let fetcher = SubscriptionFetcher::new(ReqwestHttpClient::new(device_identity)?);
    let runtime_root = data_directory.join("runtime");
    let core_logs = Arc::new(CoreLogBuffer::persistent_or_memory(
        data_directory.join("logs").join("latest-core.log"),
    ));
    let controller_factory = transactional_controller_factory(move |snapshot: &Snapshot| {
        let xray_outbound_interface = default_ipv4_interface().map_err(|_| BackendError::new())?;
        let resolved_hosts = resolve_xray_hosts(snapshot).map_err(|_| BackendError::new())?;
        let control = runtime_control
            .clone()
            .with_xray_outbound_interface(xray_outbound_interface)
            .and_then(|control| control.with_xray_resolved_hosts(resolved_hosts))
            .map_err(|_| BackendError::new())?;
        let staged = stage_runtime_with_mihomo_control(snapshot, &runtime_root, &control)
            .map_err(|_| BackendError::new())?;
        let paths = staged.paths();
        let controller = Arc::new(SidecarProcessController::new_with_logs(
            RuntimePaths {
                xray_binary: xray_binary.clone(),
                mihomo_binary: mihomo_binary.clone(),
                xray_config: paths.xray_config.clone(),
                mihomo_config: paths.mihomo_config.clone(),
            },
            core_logs.clone(),
        ));
        Ok(PreparedController::with_runtime(controller, staged))
    });
    let backend =
        CoreBackend::new_transactional_with_selector(store, fetcher, controller_factory, selector)?;

    let app = try_router(backend, token)?;
    let listener = bind_loopback(address).await?;
    let resolved_address = listener.local_addr()?;
    {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        publish_readiness_if_enabled(
            &mut stdout,
            resolved_address,
            env::var_os("MULTICORE_DAEMON_READY_STDOUT").as_deref(),
        )?;
    }
    axum::serve(listener, app).await?;
    Ok(())
}

fn required_env(name: &str) -> io::Result<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("задайте переменную окружения {name} перед запуском daemon"),
            )
        })
}

fn resolve_xray_hosts(snapshot: &Snapshot) -> io::Result<BTreeMap<String, Vec<IpAddr>>> {
    let domains = xray_outbound_server_domains(snapshot)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    resolve_xray_hosts_with(domains, |domain| {
        Ok((domain, 0)
            .to_socket_addrs()?
            .map(|address| address.ip())
            .collect())
    })
}

fn resolve_xray_hosts_with<I, F>(
    domains: I,
    mut resolver: F,
) -> io::Result<BTreeMap<String, Vec<IpAddr>>>
where
    I: IntoIterator<Item = String>,
    F: FnMut(&str) -> io::Result<Vec<IpAddr>>,
{
    let mut resolved = BTreeMap::new();
    for domain in domains {
        let mut addresses = resolver(&domain)?;
        addresses.sort_unstable();
        addresses.dedup();
        if addresses.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Xray outbound domain resolved to no IP addresses",
            ));
        }
        resolved.insert(domain, addresses);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;

    use super::resolve_xray_hosts_with;

    #[test]
    fn xray_host_bootstrap_collects_client_local_ip_answers() {
        let resolved = resolve_xray_hosts_with(
            vec!["backup.example".to_owned(), "edge.example".to_owned()],
            |domain| match domain {
                "backup.example" => Ok(vec!["198.51.100.9".parse::<IpAddr>().unwrap()]),
                "edge.example" => Ok(vec![
                    "203.0.113.7".parse::<IpAddr>().unwrap(),
                    "203.0.113.7".parse::<IpAddr>().unwrap(),
                ]),
                _ => unreachable!(),
            },
        )
        .unwrap();

        assert_eq!(
            resolved["backup.example"],
            ["198.51.100.9".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(
            resolved["edge.example"],
            ["203.0.113.7".parse::<IpAddr>().unwrap()]
        );
    }

    #[test]
    fn xray_host_bootstrap_fails_closed_when_a_domain_has_no_ip() {
        assert!(
            resolve_xray_hosts_with(vec!["edge.example".to_owned()], |_| Ok(Vec::new())).is_err()
        );
        assert!(
            resolve_xray_hosts_with(vec!["edge.example".to_owned()], |_| {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "dns"))
            })
            .is_err()
        );
    }
}
