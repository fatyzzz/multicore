//! System proxy for the TUN-off connection mode (Windows and macOS).
//!
//! While connected in that mode the user's proxy settings point at Mihomo's loopback mixed
//! port: WinINet (`HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings`) on
//! Windows, every enabled network service's web, secure web and SOCKS proxy on macOS. The daemon runs as the logged-in user (the desktop spawns it without
//! elevation), so it owns this per-user setting; the elevated core host never touches it.
//!
//! The previous values are saved to a marker file *before* anything is changed, so they come
//! back on disconnect, on a failed connect, and — through [`recover`] at daemon start — after a
//! crash. A restore only happens while the settings still hold exactly what was applied: if the
//! user changed the proxy meanwhile, their change is kept and only the marker is dropped.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

/// Hosts that must never go through the proxy: loopback and private networks.
pub const PROXY_OVERRIDE: &str = "<local>;localhost;127.*;10.*;172.16.*;192.168.*";

/// The proxy-related WinINet values. `None` means the value is absent from the registry.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProxySettings {
    pub proxy_enable: Option<u32>,
    pub proxy_server: Option<String>,
    pub proxy_override: Option<String>,
    /// A PAC URL wins over a manual proxy, so it is removed while connected and put back after.
    pub auto_config_url: Option<String>,
    /// macOS keeps proxies per network service: what each enabled service had, so every one is
    /// put back as it was. Empty on Windows and in the applied settings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub services: Vec<NetworkServiceProxy>,
}

/// One macOS network service's proxy configuration.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct NetworkServiceProxy {
    pub name: String,
    pub web: ProxyEndpoint,
    pub secure_web: ProxyEndpoint,
    pub socks: ProxyEndpoint,
    pub bypass_domains: Vec<String>,
    pub auto_proxy_url: Option<String>,
    pub auto_proxy_enabled: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProxyEndpoint {
    pub enabled: bool,
    pub server: String,
    pub port: u16,
}

impl ProxySettings {
    /// The settings MultiCore applies for a Mihomo mixed port on `127.0.0.1:<port>`.
    pub fn applied(port: u16) -> Self {
        Self {
            proxy_enable: Some(1),
            proxy_server: Some(applied_server(port)),
            proxy_override: Some(PROXY_OVERRIDE.to_owned()),
            auto_config_url: None,
            services: Vec::new(),
        }
    }
}

fn applied_server(port: u16) -> String {
    format!("127.0.0.1:{port}")
}

/// What is saved before the proxy is applied.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProxyMarker {
    pub previous: ProxySettings,
    pub applied_server: String,
}

/// The user's proxy settings store.
pub trait InternetSettings: Send + Sync {
    fn read(&self) -> io::Result<ProxySettings>;
    fn write(&self, settings: &ProxySettings) -> io::Result<()>;
    /// Tells running applications that the settings changed.
    fn notify_changed(&self);
}

/// The settings to write back, or `None` when the user has changed the proxy since it was
/// applied (their change wins).
pub fn restore_plan(current: &ProxySettings, marker: &ProxyMarker) -> Option<ProxySettings> {
    (current.proxy_enable == Some(1)
        && current.proxy_server.as_deref() == Some(marker.applied_server.as_str()))
    .then(|| marker.previous.clone())
}

/// Restores the settings saved in `marker_path`, if any, and removes the marker.
pub fn recover(settings: &dyn InternetSettings, marker_path: &Path) -> io::Result<()> {
    let bytes = match fs::read(marker_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    // An unreadable marker cannot say what to restore; keeping it would only block every
    // later connect, so it is dropped.
    if let Ok(marker) = serde_json::from_slice::<ProxyMarker>(&bytes) {
        let current = settings.read()?;
        if let Some(previous) = restore_plan(&current, &marker) {
            settings.write(&previous)?;
            settings.notify_changed();
        }
    }
    match fs::remove_file(marker_path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Applies and restores the system proxy for one proxy-mode connection.
pub struct SystemProxy {
    settings: Box<dyn InternetSettings>,
    marker_path: PathBuf,
    port: u16,
    tunnel: Option<multicore_core::TunnelProxy>,
}

impl SystemProxy {
    pub fn new(settings: Box<dyn InternetSettings>, marker_path: PathBuf, port: u16) -> Self {
        Self {
            settings,
            marker_path,
            port,
            tunnel: None,
        }
    }

    /// While this proxy is applied, `tunnel` names the mixed port, so the daemon's own
    /// subscription downloads go through the connection too (see `TunnelProxy`).
    #[must_use]
    pub fn publishing_to(mut self, tunnel: multicore_core::TunnelProxy) -> Self {
        self.tunnel = Some(tunnel);
        self
    }

    fn publish(&self, applied: bool) {
        if let Some(tunnel) = &self.tunnel {
            tunnel.set(
                applied.then(|| {
                    std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, self.port))
                }),
            );
        }
    }

    pub fn apply(&self) -> io::Result<()> {
        // A marker left by a crash still holds the user's real settings: restore them first so
        // they, not our own leftover proxy, are what gets saved now.
        recover(self.settings.as_ref(), &self.marker_path)?;
        let marker = ProxyMarker {
            previous: self.settings.read()?,
            applied_server: applied_server(self.port),
        };
        write_marker(&self.marker_path, &marker)?;
        if let Err(error) = self.settings.write(&ProxySettings::applied(self.port)) {
            // Partially written values are put back by the marker.
            let _ = recover(self.settings.as_ref(), &self.marker_path);
            return Err(error);
        }
        self.settings.notify_changed();
        self.publish(true);
        Ok(())
    }

    pub fn restore(&self) -> io::Result<()> {
        self.publish(false);
        recover(self.settings.as_ref(), &self.marker_path)
    }
}

impl crate::connection_mode::ProxySwitch for SystemProxy {
    fn apply(&self) -> io::Result<()> {
        SystemProxy::apply(self)
    }

    fn restore(&self) -> io::Result<()> {
        SystemProxy::restore(self)
    }
}

fn write_marker(path: &Path, marker: &ProxyMarker) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec(marker).map_err(io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

#[cfg(windows)]
pub use windows::WinInetSettings;

#[cfg(windows)]
mod windows {
    use std::{io, ptr};

    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        Networking::WinInet::{
            INTERNET_OPTION_REFRESH, INTERNET_OPTION_SETTINGS_CHANGED, InternetSetOptionW,
        },
        System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_DWORD, REG_SZ,
            RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
        },
    };

    use super::{InternetSettings, ProxySettings};

    const KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings";

    /// The current user's WinINet settings in the registry.
    pub struct WinInetSettings;

    struct Key(HKEY);
    impl Drop for Key {
        fn drop(&mut self) {
            unsafe { RegCloseKey(self.0) };
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn open() -> io::Result<Key> {
        let mut key: HKEY = ptr::null_mut();
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                wide(KEY).as_ptr(),
                0,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                &mut key,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(Key(key))
    }

    fn query(key: &Key, name: &str) -> io::Result<Option<(u32, Vec<u8>)>> {
        let name = wide(name);
        let mut kind = 0_u32;
        let mut size = 0_u32;
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                ptr::null_mut(),
                &mut size,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let mut data = vec![0_u8; size as usize];
        let status = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut kind,
                data.as_mut_ptr(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        data.truncate(size as usize);
        Ok(Some((kind, data)))
    }

    fn query_dword(key: &Key, name: &str) -> io::Result<Option<u32>> {
        Ok(query(key, name)?.and_then(|(kind, data)| {
            (kind == REG_DWORD && data.len() >= 4)
                .then(|| u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
        }))
    }

    fn query_string(key: &Key, name: &str) -> io::Result<Option<String>> {
        Ok(query(key, name)?.and_then(|(_, data)| {
            let units: Vec<u16> = data
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| u16::from_le_bytes(*pair))
                .take_while(|unit| *unit != 0)
                .collect();
            String::from_utf16(&units).ok()
        }))
    }

    fn set(key: &Key, name: &str, kind: u32, data: &[u8]) -> io::Result<()> {
        let status = unsafe {
            RegSetValueExW(
                key.0,
                wide(name).as_ptr(),
                0,
                kind,
                data.as_ptr(),
                data.len() as u32,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    fn delete(key: &Key, name: &str) -> io::Result<()> {
        let status = unsafe { RegDeleteValueW(key.0, wide(name).as_ptr()) };
        if status != ERROR_SUCCESS && status != ERROR_FILE_NOT_FOUND {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    }

    fn put_string(key: &Key, name: &str, value: &Option<String>) -> io::Result<()> {
        match value {
            Some(value) => {
                let bytes: Vec<u8> = wide(value)
                    .iter()
                    .flat_map(|unit| unit.to_le_bytes())
                    .collect();
                set(key, name, REG_SZ, &bytes)
            }
            None => delete(key, name),
        }
    }

    impl InternetSettings for WinInetSettings {
        fn read(&self) -> io::Result<ProxySettings> {
            let key = open()?;
            Ok(ProxySettings {
                proxy_enable: query_dword(&key, "ProxyEnable")?,
                proxy_server: query_string(&key, "ProxyServer")?,
                proxy_override: query_string(&key, "ProxyOverride")?,
                auto_config_url: query_string(&key, "AutoConfigURL")?,
                services: Vec::new(),
            })
        }

        fn write(&self, settings: &ProxySettings) -> io::Result<()> {
            let key = open()?;
            match settings.proxy_enable {
                Some(value) => set(&key, "ProxyEnable", REG_DWORD, &value.to_le_bytes())?,
                None => delete(&key, "ProxyEnable")?,
            }
            put_string(&key, "ProxyServer", &settings.proxy_server)?;
            put_string(&key, "ProxyOverride", &settings.proxy_override)?;
            put_string(&key, "AutoConfigURL", &settings.auto_config_url)
        }

        fn notify_changed(&self) {
            unsafe {
                InternetSetOptionW(
                    ptr::null(),
                    INTERNET_OPTION_SETTINGS_CHANGED,
                    ptr::null(),
                    0,
                );
                InternetSetOptionW(ptr::null(), INTERNET_OPTION_REFRESH, ptr::null(), 0);
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::MacNetworkSettings;

/// `networksetup`, which an administrator may run without `sudo` for proxy settings. Only
/// enabled services are touched; their state is read as a whole so it can be put back as is.
#[cfg(target_os = "macos")]
mod macos {
    use std::{io, process::Command};

    use super::{
        InternetSettings, NetworkServiceProxy, PROXY_OVERRIDE, ProxyEndpoint, ProxySettings,
    };

    const NETWORKSETUP: &str = "/usr/sbin/networksetup";
    /// macOS spelling of [`PROXY_OVERRIDE`]: loopback, link-local and private networks.
    const BYPASS_DOMAINS: &[&str] = &[
        "localhost",
        "*.local",
        "127.0.0.0/8",
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "169.254.0.0/16",
    ];

    pub struct MacNetworkSettings;

    fn networksetup(args: &[&str]) -> io::Result<String> {
        let output = Command::new(NETWORKSETUP).args(args).output()?;
        if !output.status.success() {
            return Err(io::Error::other(format!(
                "networksetup {} failed: {}",
                args.first().copied().unwrap_or_default(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    /// Enabled services; the first line is a legend and disabled ones start with `*`.
    fn enabled_services() -> io::Result<Vec<String>> {
        Ok(networksetup(&["-listallnetworkservices"])?
            .lines()
            .skip(1)
            .filter(|line| !line.is_empty() && !line.starts_with('*'))
            .map(str::to_owned)
            .collect())
    }

    fn field<'a>(text: &'a str, name: &str) -> Option<&'a str> {
        text.lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix(':'))
            .map(str::trim)
    }

    fn read_endpoint(flag: &str, service: &str) -> io::Result<ProxyEndpoint> {
        let text = networksetup(&[flag, service])?;
        Ok(ProxyEndpoint {
            enabled: field(&text, "Enabled") == Some("Yes"),
            server: field(&text, "Server").unwrap_or_default().to_owned(),
            port: field(&text, "Port")
                .and_then(|port| port.parse().ok())
                .unwrap_or_default(),
        })
    }

    fn read_service(name: &str) -> io::Result<NetworkServiceProxy> {
        let bypass = networksetup(&["-getproxybypassdomains", name])?;
        let auto = networksetup(&["-getautoproxyurl", name])?;
        Ok(NetworkServiceProxy {
            name: name.to_owned(),
            web: read_endpoint("-getwebproxy", name)?,
            secure_web: read_endpoint("-getsecurewebproxy", name)?,
            socks: read_endpoint("-getsocksfirewallproxy", name)?,
            // «There aren't any bypass domains set on …» when the list is empty.
            bypass_domains: bypass
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty() && !line.contains(' '))
                .map(str::to_owned)
                .collect(),
            auto_proxy_url: field(&auto, "URL")
                .filter(|url| *url != "(null)" && !url.is_empty())
                .map(str::to_owned),
            auto_proxy_enabled: field(&auto, "Enabled") == Some("Yes"),
        })
    }

    fn write_endpoint(
        set: &str,
        state: &str,
        service: &str,
        endpoint: &ProxyEndpoint,
    ) -> io::Result<()> {
        if !endpoint.server.is_empty() && endpoint.port != 0 {
            networksetup(&[set, service, &endpoint.server, &endpoint.port.to_string()])?;
        }
        networksetup(&[state, service, if endpoint.enabled { "on" } else { "off" }])?;
        Ok(())
    }

    fn write_service(service: &NetworkServiceProxy) -> io::Result<()> {
        let name = service.name.as_str();
        write_endpoint("-setwebproxy", "-setwebproxystate", name, &service.web)?;
        write_endpoint(
            "-setsecurewebproxy",
            "-setsecurewebproxystate",
            name,
            &service.secure_web,
        )?;
        write_endpoint(
            "-setsocksfirewallproxy",
            "-setsocksfirewallproxystate",
            name,
            &service.socks,
        )?;
        let mut bypass = vec!["-setproxybypassdomains", name];
        if service.bypass_domains.is_empty() {
            bypass.push("Empty");
        } else {
            bypass.extend(service.bypass_domains.iter().map(String::as_str));
        }
        networksetup(&bypass)?;
        if let Some(url) = &service.auto_proxy_url {
            networksetup(&["-setautoproxyurl", name, url])?;
        }
        networksetup(&[
            "-setautoproxystate",
            name,
            if service.auto_proxy_enabled {
                "on"
            } else {
                "off"
            },
        ])?;
        Ok(())
    }

    /// The settings every service gets for `server` (`127.0.0.1:<port>`).
    fn applied_service(name: &str, server: &str) -> io::Result<NetworkServiceProxy> {
        let (host, port) = server
            .rsplit_once(':')
            .and_then(|(host, port)| Some((host, port.parse().ok()?)))
            .ok_or_else(|| io::Error::other("proxy server is not host:port"))?;
        let endpoint = ProxyEndpoint {
            enabled: true,
            server: host.to_owned(),
            port,
        };
        Ok(NetworkServiceProxy {
            name: name.to_owned(),
            web: endpoint.clone(),
            secure_web: endpoint.clone(),
            socks: endpoint,
            bypass_domains: BYPASS_DOMAINS
                .iter()
                .map(|domain| (*domain).to_owned())
                .collect(),
            auto_proxy_url: None,
            auto_proxy_enabled: false,
        })
    }

    impl InternetSettings for MacNetworkSettings {
        /// `proxy_enable` and `proxy_server` summarise the services: on, with that server, only
        /// while every service that has a proxy on has all three on the same `host:port`. A
        /// service with no proxy at all (one that came up after the proxy was applied) is not
        /// counted, so it cannot keep the applied proxy from being restored.
        fn read(&self) -> io::Result<ProxySettings> {
            let services = enabled_services()?
                .iter()
                .map(|name| read_service(name))
                .collect::<io::Result<Vec<_>>>()?;
            let server_of = |service: &NetworkServiceProxy| {
                let web = &service.web;
                (web.enabled && service.secure_web == *web && service.socks == *web)
                    .then(|| format!("{}:{}", web.server, web.port))
            };
            let mut servers = services
                .iter()
                .filter(|service| {
                    service.web.enabled || service.secure_web.enabled || service.socks.enabled
                })
                .map(server_of);
            let first = servers.next().flatten();
            let shared = first.filter(|server| servers.all(|other| other.as_ref() == Some(server)));
            Ok(ProxySettings {
                proxy_enable: Some(u32::from(shared.is_some())),
                proxy_server: shared,
                proxy_override: None,
                auto_config_url: None,
                services,
            })
        }

        /// Either the applied settings (no `services`: every enabled service is pointed at
        /// `proxy_server`) or a saved state (each service written back as it was).
        fn write(&self, settings: &ProxySettings) -> io::Result<()> {
            if !settings.services.is_empty() {
                let mut first_error = None;
                for service in &settings.services {
                    if let Err(error) = write_service(service) {
                        first_error.get_or_insert(error);
                    }
                }
                return first_error.map_or(Ok(()), Err);
            }
            let server = match (settings.proxy_enable, &settings.proxy_server) {
                (Some(1), Some(server)) => server,
                _ => return Ok(()),
            };
            debug_assert_eq!(settings.proxy_override.as_deref(), Some(PROXY_OVERRIDE));
            for name in enabled_services()? {
                write_service(&applied_service(&name, server)?)?;
            }
            Ok(())
        }

        /// System Configuration tells running applications itself.
        fn notify_changed(&self) {}
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[derive(Clone, Default)]
    struct FakeSettings {
        values: Arc<Mutex<ProxySettings>>,
        notified: Arc<Mutex<u32>>,
        fail_write: bool,
    }

    impl InternetSettings for FakeSettings {
        fn read(&self) -> io::Result<ProxySettings> {
            Ok(self.values.lock().unwrap().clone())
        }
        fn write(&self, settings: &ProxySettings) -> io::Result<()> {
            if self.fail_write {
                return Err(io::Error::other("denied"));
            }
            *self.values.lock().unwrap() = settings.clone();
            Ok(())
        }
        fn notify_changed(&self) {
            *self.notified.lock().unwrap() += 1;
        }
    }

    fn user_settings() -> ProxySettings {
        ProxySettings {
            proxy_enable: Some(0),
            proxy_server: Some("corp:3128".into()),
            proxy_override: None,
            auto_config_url: Some("http://wpad/proxy.pac".into()),
            services: Vec::new(),
        }
    }

    fn proxy(settings: &FakeSettings, directory: &Path) -> SystemProxy {
        SystemProxy::new(
            Box::new(settings.clone()),
            directory.join("system-proxy.json"),
            28765,
        )
    }

    #[test]
    fn apply_saves_the_user_settings_then_points_at_the_mixed_port() {
        let directory = tempfile::tempdir().unwrap();
        let settings = FakeSettings::default();
        *settings.values.lock().unwrap() = user_settings();
        let proxy = proxy(&settings, directory.path());

        proxy.apply().unwrap();

        assert_eq!(
            *settings.values.lock().unwrap(),
            ProxySettings::applied(28765)
        );
        let marker: ProxyMarker =
            serde_json::from_slice(&fs::read(directory.path().join("system-proxy.json")).unwrap())
                .unwrap();
        assert_eq!(marker.previous, user_settings());
        assert_eq!(marker.applied_server, "127.0.0.1:28765");
        assert_eq!(*settings.notified.lock().unwrap(), 1);

        proxy.restore().unwrap();
        assert_eq!(*settings.values.lock().unwrap(), user_settings());
        assert!(!directory.path().join("system-proxy.json").exists());
    }

    /// While connected in proxy mode the daemon's own downloads go through the mixed port;
    /// once disconnected (or if applying failed) they go directly again.
    #[test]
    fn the_mixed_port_is_published_only_while_the_proxy_is_applied() {
        let directory = tempfile::tempdir().unwrap();
        let settings = FakeSettings::default();
        *settings.values.lock().unwrap() = user_settings();
        let tunnel = multicore_core::TunnelProxy::default();
        let proxy = proxy(&settings, directory.path()).publishing_to(tunnel.clone());
        assert_eq!(tunnel.get(), None);

        proxy.apply().unwrap();
        assert_eq!(tunnel.get(), Some("127.0.0.1:28765".parse().unwrap()));

        proxy.restore().unwrap();
        assert_eq!(tunnel.get(), None);
    }

    #[test]
    fn restore_keeps_a_proxy_the_user_changed_meanwhile() {
        let marker = ProxyMarker {
            previous: user_settings(),
            applied_server: "127.0.0.1:28765".into(),
        };
        let applied = ProxySettings::applied(28765);
        assert_eq!(restore_plan(&applied, &marker), Some(user_settings()));
        let changed_server = ProxySettings {
            proxy_server: Some("other:8080".into()),
            ..applied.clone()
        };
        assert_eq!(restore_plan(&changed_server, &marker), None);
        let switched_off = ProxySettings {
            proxy_enable: Some(0),
            ..applied
        };
        assert_eq!(restore_plan(&switched_off, &marker), None);
    }

    #[test]
    fn recover_after_a_crash_restores_once_and_drops_the_marker() {
        let directory = tempfile::tempdir().unwrap();
        let settings = FakeSettings::default();
        *settings.values.lock().unwrap() = user_settings();
        proxy(&settings, directory.path()).apply().unwrap();
        let marker_path = directory.path().join("system-proxy.json");

        recover(&settings, &marker_path).unwrap();
        assert_eq!(*settings.values.lock().unwrap(), user_settings());
        assert!(!marker_path.exists());
        // No marker: nothing to do.
        recover(&settings, &marker_path).unwrap();
        assert_eq!(*settings.values.lock().unwrap(), user_settings());
    }

    #[test]
    fn apply_after_an_unrecovered_crash_saves_the_real_user_settings() {
        let directory = tempfile::tempdir().unwrap();
        let settings = FakeSettings::default();
        *settings.values.lock().unwrap() = user_settings();
        proxy(&settings, directory.path()).apply().unwrap();
        // The daemon died while connected; the next session applies again.
        let next = SystemProxy::new(
            Box::new(settings.clone()),
            directory.path().join("system-proxy.json"),
            31000,
        );
        next.apply().unwrap();
        next.restore().unwrap();
        assert_eq!(*settings.values.lock().unwrap(), user_settings());
    }

    #[test]
    fn a_failed_apply_leaves_the_user_settings_and_no_marker() {
        let directory = tempfile::tempdir().unwrap();
        let settings = FakeSettings {
            fail_write: true,
            ..FakeSettings::default()
        };
        *settings.values.lock().unwrap() = user_settings();

        assert!(proxy(&settings, directory.path()).apply().is_err());
        assert_eq!(*settings.values.lock().unwrap(), user_settings());
        // Nothing was applied, so the rollback has nothing to write and just drops the marker.
        assert!(!directory.path().join("system-proxy.json").exists());
    }

    #[test]
    fn an_unreadable_marker_is_dropped_without_touching_settings() {
        let directory = tempfile::tempdir().unwrap();
        let marker_path = directory.path().join("system-proxy.json");
        fs::write(&marker_path, b"not json").unwrap();
        let settings = FakeSettings::default();
        *settings.values.lock().unwrap() = user_settings();

        recover(&settings, &marker_path).unwrap();
        assert_eq!(*settings.values.lock().unwrap(), user_settings());
        assert!(!marker_path.exists());
    }
}
