use std::io;

#[derive(Clone, Debug, PartialEq, Eq)]
struct DefaultRouteCandidate {
    alias: String,
    interface_index: u32,
    route_metric: u32,
    interface_metric: u32,
    prefix_length: u8,
    connected: bool,
    loopback: bool,
}

fn select_default_route(
    candidates: impl IntoIterator<Item = DefaultRouteCandidate>,
) -> io::Result<String> {
    candidates
        .into_iter()
        .filter(|candidate| {
            candidate.prefix_length == 0
                && candidate.connected
                && !candidate.loopback
                && candidate.alias != "MultiCore"
                && !candidate.alias.is_empty()
                && candidate.alias.len() <= 256
                && !candidate.alias.chars().any(char::is_control)
        })
        .min_by_key(|candidate| {
            (
                candidate
                    .route_metric
                    .saturating_add(candidate.interface_metric),
                candidate.interface_index,
            )
        })
        .map(|candidate| candidate.alias)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "Windows has no safe connected IPv4 default interface",
            )
        })
}

#[cfg(windows)]
pub fn default_ipv4_interface() -> io::Result<String> {
    use std::{ffi::c_void, ptr, slice};

    use windows_sys::Win32::{
        Foundation::NO_ERROR,
        NetworkManagement::{
            IpHelper::{
                ConvertInterfaceLuidToAlias, FreeMibTable, GetIpForwardTable2, GetIpInterfaceEntry,
                MIB_IPFORWARD_TABLE2, MIB_IPINTERFACE_ROW,
            },
            Ndis::IF_MAX_STRING_SIZE,
        },
        Networking::WinSock::AF_INET,
    };

    struct MibTable(*const c_void);
    impl Drop for MibTable {
        fn drop(&mut self) {
            // SAFETY: the pointer is returned by GetIpForwardTable2 and freed exactly once.
            unsafe { FreeMibTable(self.0) };
        }
    }

    let mut table: *mut MIB_IPFORWARD_TABLE2 = ptr::null_mut();
    // SAFETY: `table` is a valid out-pointer and AF_INET requests the IPv4 route table.
    let result = unsafe { GetIpForwardTable2(AF_INET, &mut table) };
    if result != NO_ERROR {
        return Err(io::Error::from_raw_os_error(result as i32));
    }
    if table.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "Windows returned an empty IPv4 route table",
        ));
    }
    let _table = MibTable(table.cast());
    // SAFETY: Windows allocates `NumEntries` contiguous rows beginning at `Table`.
    let rows =
        unsafe { slice::from_raw_parts((*table).Table.as_ptr(), (*table).NumEntries as usize) };
    let mut candidates = Vec::new();
    for row in rows {
        let mut interface = MIB_IPINTERFACE_ROW {
            Family: AF_INET,
            InterfaceLuid: row.InterfaceLuid,
            ..Default::default()
        };
        // SAFETY: `interface` is initialized with family/LUID and is a valid writable row.
        if unsafe { GetIpInterfaceEntry(&mut interface) } != NO_ERROR {
            continue;
        }
        let mut alias = [0_u16; IF_MAX_STRING_SIZE as usize + 1];
        // SAFETY: the alias buffer is writable and its length matches the provided capacity.
        if unsafe {
            ConvertInterfaceLuidToAlias(&row.InterfaceLuid, alias.as_mut_ptr(), alias.len())
        } != NO_ERROR
        {
            continue;
        }
        let alias_length = alias
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(alias.len());
        let Ok(alias) = String::from_utf16(&alias[..alias_length]) else {
            continue;
        };
        candidates.push(DefaultRouteCandidate {
            alias,
            interface_index: row.InterfaceIndex,
            route_metric: row.Metric,
            interface_metric: interface.Metric,
            prefix_length: row.DestinationPrefix.PrefixLength,
            connected: interface.Connected,
            loopback: row.Loopback,
        });
    }
    select_default_route(candidates)
}

#[cfg(not(windows))]
pub fn default_ipv4_interface() -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "automatic Xray outbound interface binding requires Windows",
    ))
}

#[cfg(test)]
mod tests {
    use super::{DefaultRouteCandidate, default_ipv4_interface, select_default_route};

    fn candidate(alias: &str, route_metric: u32, interface_metric: u32) -> DefaultRouteCandidate {
        DefaultRouteCandidate {
            alias: alias.to_owned(),
            interface_index: 6,
            route_metric,
            interface_metric,
            prefix_length: 0,
            connected: true,
            loopback: false,
        }
    }

    #[test]
    fn route_selection_uses_the_best_connected_default_outside_multicore() {
        let mut disconnected = candidate("Wi-Fi", 1, 1);
        disconnected.connected = false;
        let mut specific = candidate("Specific", 0, 0);
        specific.prefix_length = 24;
        let mut loopback = candidate("Loopback", 0, 0);
        loopback.loopback = true;

        assert_eq!(
            select_default_route(vec![
                candidate("MultiCore", 0, 0),
                disconnected,
                specific,
                loopback,
                candidate("Ethernet", 20, 15),
                candidate("Corporate VPN", 5, 10),
            ])
            .unwrap(),
            "Corporate VPN"
        );
    }

    #[test]
    fn route_selection_fails_without_one_safe_connected_default() {
        assert!(select_default_route(Vec::new()).is_err());
        let mut unsafe_alias = candidate("Ethernet\nInjected", 1, 1);
        unsafe_alias.interface_index = 7;
        assert!(select_default_route(vec![unsafe_alias]).is_err());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "reads the host Windows route table"]
    fn host_route_probe_returns_a_safe_interface_alias() {
        let alias = default_ipv4_interface().unwrap();
        assert!(!alias.is_empty());
        assert!(!alias.chars().any(char::is_control));
    }
}
