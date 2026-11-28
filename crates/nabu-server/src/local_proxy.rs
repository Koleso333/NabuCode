use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

use ncore_tokio::Proxy;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Socks5,
    Http,
    Socks5Login,
}

impl Kind {
    fn scheme(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Socks5 | Self::Socks5Login => "socks5",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Socks5 => 0,
            Self::Http => 2,
            Self::Socks5Login => 4,
        }
    }
}

#[derive(Debug)]
struct Found {
    address: SocketAddr,
    kind: Kind,
    system: bool,
}

impl Found {
    fn url(&self) -> String {
        format!("{}://{}", self.kind.scheme(), self.address)
    }
}

const CLIENTS: &[&str] = &[
    "xray",
    "v2ray",
    "v2rayn",
    "sing-box",
    "singbox",
    "mihomo",
    "clash",
    "clash-meta",
    "verge-mihomo",
    "clash-verge",
    "nekoray",
    "nekobox",
    "throne",
    "hiddify",
    "hiddifycli",
    "happ",
    "karing",
    "sslocal",
    "ss-local",
    "shadowsocks",
    "trojan",
    "trojan-go",
    "naive",
    "hysteria",
    "hysteria2",
    "tuic",
    "tuic-client",
    "juicity",
    "brook",
    "gost",
    "outline",
    "psiphon",
    "lantern",
    "privoxy",
    "3proxy",
    "invisible man",
    "amnezia",
];

fn is_client(name: &str) -> bool {
    CLIENTS.iter().any(|client| {
        name == *client
            || name
                .strip_prefix(client)
                .is_some_and(|suffix| suffix.starts_with(['-', '_']))
    })
}

pub(crate) fn resolve() -> Option<Proxy> {
    let explicit = ["HTTPS_PROXY", "https_proxy", "ALL_PROXY", "all_proxy"]
        .iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    select_url(explicit.as_deref(), find).and_then(|url| Proxy::parse(&url).ok())
}

fn select_url(explicit: Option<&str>, discover: impl FnOnce() -> Vec<Found>) -> Option<String> {
    let explicit = explicit
        .map(str::trim)
        .filter(|url| Proxy::parse(url).is_ok());
    if let Some(url) = explicit {
        if explicit_works(url) {
            return Some(url.to_owned());
        }
        eprintln!("[server] configured local proxy is unavailable; looking for a running VPN");
    }
    let Some(found) = discover()
        .into_iter()
        .find(|found| found.kind != Kind::Socks5Login)
    else {
        return explicit.map(str::to_owned);
    };
    let url = found.url();
    eprintln!("[server] detected local proxy: {url}");
    Some(url)
}

fn explicit_works(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    let (auth, authority) = rest
        .trim_end_matches('/')
        .rsplit_once('@')
        .map_or((false, rest.trim_end_matches('/')), |(_, authority)| {
            (true, authority)
        });
    let Some((host, port)) = authority.rsplit_once(':') else {
        return false;
    };
    let host = host.trim_matches(['[', ']']);
    let local = host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback());
    if !local {
        return true;
    }
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    let Ok(addresses) = (host, port).to_socket_addrs() else {
        return false;
    };
    addresses
        .filter(|address| address.ip().is_loopback())
        .any(|address| {
            if scheme.eq_ignore_ascii_case("http") {
                probe_http(address)
            } else {
                match probe_socks(address) {
                    Some(Kind::Socks5) => true,
                    Some(Kind::Socks5Login) => auth,
                    _ => false,
                }
            }
        })
}

fn connect(address: SocketAddr) -> Option<TcpStream> {
    let stream = TcpStream::connect_timeout(&address, Duration::from_millis(500)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(1500)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    Some(stream)
}

fn probe_socks(address: SocketAddr) -> Option<Kind> {
    let mut stream = connect(address)?;
    stream.write_all(&[5, 1, 0]).ok()?;
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).ok()?;
    match reply {
        [5, 0] => Some(Kind::Socks5),
        [5, 2] => Some(Kind::Socks5Login),
        [5, 0xff] => {
            let mut stream = connect(address)?;
            stream.write_all(&[5, 1, 2]).ok()?;
            stream.read_exact(&mut reply).ok()?;
            (reply == [5, 2]).then_some(Kind::Socks5Login)
        }
        _ => None,
    }
}

fn probe_http(address: SocketAddr) -> bool {
    let Some(mut stream) = connect(address) else {
        return false;
    };
    if stream
        .write_all(b"CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut head = [0; 8];
    stream.read_exact(&mut head).is_ok() && head.starts_with(b"HTTP/1.")
}

fn find_candidates(listeners: Vec<(SocketAddr, String)>, system_ports: &[u16]) -> Vec<Found> {
    let mut seen = std::collections::HashSet::new();
    let probes: Vec<_> = listeners
        .into_iter()
        .filter_map(|(address, name)| {
            let system = system_ports.contains(&address.port());
            if (!system && !is_client(&name)) || !seen.insert(address) {
                return None;
            }
            Some(std::thread::spawn(move || {
                let kind =
                    probe_socks(address).or_else(|| probe_http(address).then_some(Kind::Http))?;
                Some(Found {
                    address,
                    kind,
                    system,
                })
            }))
        })
        .collect();
    let mut found: Vec<_> = probes
        .into_iter()
        .filter_map(|probe| probe.join().ok().flatten())
        .collect();
    found.sort_by_key(|found| (found.kind.rank() + u8::from(!found.system), found.address));
    found
}

#[cfg(windows)]
fn find() -> Vec<Found> {
    find_candidates(windows::listeners(), &windows::system_ports())
}

#[cfg(not(windows))]
fn find() -> Vec<Found> {
    Vec::new()
}

#[cfg(windows)]
mod windows {
    use std::ffi::c_void;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    #[link(name = "iphlpapi")]
    unsafe extern "system" {
        fn GetExtendedTcpTable(
            table: *mut c_void,
            size: *mut u32,
            order: i32,
            af: u32,
            class: i32,
            reserved: u32,
        ) -> u32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn QueryFullProcessImageNameW(
            process: isize,
            flags: u32,
            name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn RegGetValueW(
            key: isize,
            sub_key: *const u16,
            value: *const u16,
            flags: u32,
            kind: *mut u32,
            data: *mut c_void,
            size: *mut u32,
        ) -> i32;
    }

    fn table(family: u32) -> Vec<u32> {
        let mut size = 0;
        unsafe { GetExtendedTcpTable(std::ptr::null_mut(), &mut size, 0, family, 3, 0) };
        for _ in 0..3 {
            let mut buffer = vec![0u32; (size as usize).div_ceil(4).max(1) + 64];
            size = (buffer.len() * 4) as u32;
            let rc = unsafe {
                GetExtendedTcpTable(buffer.as_mut_ptr().cast(), &mut size, 0, family, 3, 0)
            };
            if rc == 0 {
                return buffer;
            }
            if rc != 122 {
                break;
            }
        }
        Vec::new()
    }

    pub(super) fn listeners() -> Vec<(SocketAddr, String)> {
        let mut listeners = Vec::new();
        let mut names = std::collections::HashMap::new();
        for family in [2, 23] {
            let table = table(family);
            let Some(&count) = table.first() else {
                continue;
            };
            for row in table[1..]
                .chunks_exact(if family == 2 { 6 } else { 14 })
                .take(count as usize)
            {
                let (ip, port, pid) = if family == 2 {
                    (
                        IpAddr::V4(Ipv4Addr::from(row[1].to_ne_bytes())),
                        row[2],
                        row[5],
                    )
                } else {
                    let mut bytes = [0; 16];
                    for (chunk, word) in bytes.chunks_exact_mut(4).zip(&row[..4]) {
                        chunk.copy_from_slice(&word.to_ne_bytes());
                    }
                    (IpAddr::V6(Ipv6Addr::from(bytes)), row[5], row[13])
                };
                if !ip.is_loopback() && !ip.is_unspecified() {
                    continue;
                }
                let ip = match ip {
                    IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                    ip => ip,
                };
                let name = names
                    .entry(pid)
                    .or_insert_with(|| process_name(pid).unwrap_or_default());
                listeners.push((SocketAddr::new(ip, u16::from_be(port as u16)), name.clone()));
            }
        }
        listeners
    }

    fn process_name(pid: u32) -> Option<String> {
        unsafe {
            let handle = OpenProcess(0x1000, 0, pid);
            if handle == 0 {
                return None;
            }
            let mut buffer = [0u16; 32768];
            let mut size = buffer.len() as u32;
            let ok = QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size);
            CloseHandle(handle);
            if ok == 0 {
                return None;
            }
            let path = String::from_utf16_lossy(&buffer[..size as usize]);
            let file = path.rsplit('\\').next()?.to_lowercase();
            Some(file.strip_suffix(".exe").unwrap_or(&file).to_owned())
        }
    }

    fn reg(value: &str, flags: u32, data: &mut [u8]) -> Option<usize> {
        let key: Vec<u16> = r"Software\Microsoft\Windows\CurrentVersion\Internet Settings"
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let name: Vec<u16> = value.encode_utf16().chain(Some(0)).collect();
        let mut size = data.len() as u32;
        let rc = unsafe {
            RegGetValueW(
                0x8000_0001u32 as i32 as isize,
                key.as_ptr(),
                name.as_ptr(),
                flags,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &mut size,
            )
        };
        (rc == 0 && size as usize <= data.len()).then_some(size as usize)
    }

    pub(super) fn system_ports() -> Vec<u16> {
        let mut enabled = [0; 4];
        if reg("ProxyEnable", 0x10, &mut enabled).is_none() || u32::from_le_bytes(enabled) == 0 {
            return Vec::new();
        }
        let mut raw = [0; 8192];
        let Some(size) = reg("ProxyServer", 0x2, &mut raw) else {
            return Vec::new();
        };
        let units: Vec<u16> = raw[..size]
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .collect();
        system_ports_from(&String::from_utf16_lossy(&units))
    }

    pub(super) fn system_ports_from(server: &str) -> Vec<u16> {
        server
            .trim_end_matches('\0')
            .split(';')
            .filter_map(|part| {
                let address = part.rsplit('=').next()?.trim().split("://").last()?;
                let (host, port) = address.rsplit_once(':')?;
                let host = host.trim_matches(['[', ']']);
                (host.eq_ignore_ascii_case("localhost")
                    || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback()))
                .then(|| port.parse().ok())
                .flatten()
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread::{self, JoinHandle};
    use std::time::Instant;

    fn mock_proxy(socks: bool) -> (SocketAddr, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let task = thread::spawn(move || {
            for _ in 0..if socks { 1 } else { 2 } {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(Instant::now() < deadline, "proxy was never probed");
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut greeting = [0; 3];
                stream.read_exact(&mut greeting).unwrap();
                if greeting == [5, 1, 0] {
                    if socks {
                        stream.write_all(&[5, 0]).unwrap();
                    }
                } else {
                    assert!(!socks);
                    let mut request = greeting.to_vec();
                    let mut byte = [0];
                    while !request.ends_with(b"\r\n\r\n") {
                        stream.read_exact(&mut byte).unwrap();
                        request.push(byte[0]);
                    }
                    assert_eq!(
                        request,
                        b"CONNECT 127.0.0.1:9 HTTP/1.1\r\nHost: 127.0.0.1:9\r\n\r\n"
                    );
                    stream
                        .write_all(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
                        .unwrap();
                }
            }
        });
        (address, task)
    }

    #[test]
    fn discovers_arbitrary_socks_and_http_ports_and_ranks_them() {
        let (socks, socks_task) = mock_proxy(true);
        let (system_socks, system_task) = mock_proxy(true);
        let (http, http_task) = mock_proxy(false);
        let found = find_candidates(
            vec![
                (http, "mihomo-windows-amd64".into()),
                (socks, "xray".into()),
                (system_socks, "unknown-system-proxy".into()),
                (socks, "xray".into()),
            ],
            &[system_socks.port()],
        );
        socks_task.join().unwrap();
        system_task.join().unwrap();
        http_task.join().unwrap();
        assert_eq!(found.len(), 3);
        assert_eq!(found[0].address, system_socks);
        assert_eq!(found[1].address, socks);
        assert_eq!(found[2].address, http);
        assert_eq!(found[2].kind, Kind::Http);
    }

    #[test]
    fn stale_local_proxy_falls_back_to_discovery() {
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let stale = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let (address, task) = mock_proxy(true);
        let selected = select_url(Some(&stale), || {
            find_candidates(vec![(address, "sing-box".into())], &[])
        });
        task.join().unwrap();
        assert_eq!(selected, Some(format!("socks5://{address}")));
    }

    #[test]
    fn working_explicit_local_proxy_keeps_priority() {
        let (address, task) = mock_proxy(true);
        let url = format!("socks5://{address}");
        assert_eq!(
            select_url(Some(&url), || panic!("explicit proxy was ignored")),
            Some(url)
        );
        task.join().unwrap();
    }

    #[test]
    fn unavailable_explicit_proxy_does_not_silently_switch_to_direct() {
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        assert_eq!(select_url(Some(&url), Vec::new), Some(url));
    }

    #[test]
    fn explicit_external_proxy_is_preserved_without_scanning() {
        let url = "http://user:password@proxy.example:8080";
        assert_eq!(
            select_url(Some(url), || panic!("external proxy was ignored")),
            Some(url.into())
        );
    }

    #[test]
    fn proxies_requiring_credentials_are_not_automatically_selected() {
        assert!(
            select_url(None, || vec![Found {
                address: "127.0.0.1:2080".parse().unwrap(),
                kind: Kind::Socks5Login,
                system: true,
            }])
            .is_none()
        );
    }

    #[test]
    fn port_2080_and_ipv6_urls_are_supported() {
        for address in ["127.0.0.1:2080", "[::1]:2080"] {
            let selected = select_url(Some("invalid"), || {
                vec![Found {
                    address: address.parse().unwrap(),
                    kind: Kind::Socks5,
                    system: false,
                }]
            })
            .unwrap();
            assert_eq!(selected, format!("socks5://{address}"));
            assert!(Proxy::parse(&selected).is_ok());
        }
    }

    #[test]
    fn unrelated_processes_are_not_probed() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(
            find_candidates(vec![(listener.local_addr().unwrap(), "chrome".into())], &[])
                .is_empty()
        );
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert!(is_client("happ"));
        assert!(is_client("sing-box"));
        assert!(!is_client("xrayish"));
    }

    #[cfg(windows)]
    #[test]
    fn parses_all_local_windows_proxy_entries() {
        assert_eq!(
            windows::system_ports_from(
                "http=127.0.0.1:2080;https=localhost:7890;socks=[::1]:1080\0"
            ),
            vec![2080, 7890, 1080]
        );
        assert_eq!(
            windows::system_ports_from("http://127.0.0.1:2080"),
            vec![2080]
        );
        assert!(windows::system_ports_from("proxy.example:8080").is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn enumerates_ipv4_and_ipv6_listeners() {
        let ipv4 = TcpListener::bind("127.0.0.1:0").unwrap();
        let ipv6 = TcpListener::bind("[::1]:0").unwrap();
        let listeners = windows::listeners();
        for address in [ipv4.local_addr().unwrap(), ipv6.local_addr().unwrap()] {
            assert!(
                listeners
                    .iter()
                    .any(|(found, name)| *found == address && !name.is_empty()),
                "missing listener: {address}"
            );
        }
    }

    #[test]
    #[ignore = "probes the currently running local VPN without external requests"]
    fn detects_running_local_vpn() {
        let found = find();
        assert!(!found.is_empty(), "no running local VPN detected");
        for proxy in found {
            eprintln!(
                "{} ({:?}, system={})",
                proxy.url(),
                proxy.kind,
                proxy.system
            );
        }
    }
}
