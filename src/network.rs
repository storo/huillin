//! All BitTorrent sockets and DNS pass through an immutable routing policy.
//! A selected interface never falls back to the system's default route.
use hickory_proto::op::{Message, MessageType, Query, ResponseCode};
use hickory_proto::rr::{Name, RData, RecordType};
use network_interface::{NetworkInterface, NetworkInterfaceConfig};
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{lookup_host, TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::sync::watch;
use tokio::time::{sleep, timeout};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Empty means unrestricted. A missing named interface remains blocked.
    pub interface: String,
    pub dns: IpAddr,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            interface: String::new(),
            dns: IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Adapter {
    pub name: String,
    pub index: u32,
    pub addresses: Vec<IpAddr>,
}

pub fn interfaces() -> io::Result<Vec<Adapter>> {
    let mut adapters: Vec<_> = NetworkInterface::show()
        .map_err(io::Error::other)?
        .into_iter()
        .filter(|i| !i.internal && i.index != 0)
        .map(|i| {
            let mut addresses: Vec<_> = i.addr.iter().map(|a| a.ip()).filter(usable).collect();
            addresses.sort();
            addresses.dedup();
            Adapter {
                name: i.name,
                index: i.index,
                addresses,
            }
        })
        .collect();
    adapters.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(adapters)
}

fn usable(ip: &IpAddr) -> bool {
    !ip.is_unspecified()
        && !ip.is_loopback()
        && !ip.is_multicast()
        && match ip {
            IpAddr::V4(a) => !a.is_link_local(),
            IpAddr::V6(a) => !a.is_unicast_link_local() && a.to_ipv4_mapped().is_none(),
        }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    pub config: Config,
    adapter: Option<Adapter>,
    pub error: Option<String>,
}

impl Route {
    fn discover(config: Config) -> Self {
        let mut route = Self {
            config,
            adapter: None,
            error: None,
        };
        if !route.config.interface.is_empty() {
            match interfaces() {
                Ok(adapters) => {
                    route.adapter = adapters
                        .into_iter()
                        .find(|a| a.name == route.config.interface && !a.addresses.is_empty());
                    if route.adapter.is_none() {
                        route.error =
                            Some("Esperando la interfaz seleccionada; tráfico bloqueado".into());
                    }
                }
                Err(e) => route.error = Some(format!("No se pueden consultar las interfaces: {e}")),
            }
            // Probe OS enforcement, including permission/unsupported-option errors.
            if route.error.is_none() {
                let v6 = route.config.dns.is_ipv6();
                if let Err(e) = route.udp(v6) {
                    route.error = Some(format!("Tráfico bloqueado: {e}"));
                }
            }
        }
        route
    }

    pub fn ready(&self) -> bool {
        self.error.is_none()
    }

    fn local(&self, v6: bool, port: u16) -> io::Result<SocketAddr> {
        if let Some(error) = &self.error {
            return Err(io::Error::new(io::ErrorKind::NotConnected, error.clone()));
        }
        let ip = if self.config.interface.is_empty() {
            if v6 {
                IpAddr::V6(Ipv6Addr::UNSPECIFIED)
            } else {
                IpAddr::V4(Ipv4Addr::UNSPECIFIED)
            }
        } else {
            self.adapter
                .as_ref()
                .and_then(|a| a.addresses.iter().find(|ip| ip.is_ipv6() == v6))
                .copied()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::AddrNotAvailable,
                        "La interfaz no tiene una dirección de esta familia",
                    )
                })?
        };
        Ok(SocketAddr::new(ip, port))
    }

    fn socket(&self, v6: bool, kind: Type, port: u16) -> io::Result<Socket> {
        let local = self.local(v6, port)?;
        let protocol = if kind == Type::STREAM {
            Protocol::TCP
        } else {
            Protocol::UDP
        };
        let sock = Socket::new(
            if v6 { Domain::IPV6 } else { Domain::IPV4 },
            kind,
            Some(protocol),
        )?;
        if v6 {
            sock.set_only_v6(true)?;
        }
        if let Some(adapter) = &self.adapter {
            pin(&sock, adapter, v6)?;
        }
        sock.set_nonblocking(true)?;
        sock.bind(&local.into())?;
        Ok(sock)
    }

    pub fn udp(&self, v6: bool) -> io::Result<UdpSocket> {
        UdpSocket::from_std(self.socket(v6, Type::DGRAM, 0)?.into())
    }

    pub fn listen(&self, v6: bool, port: u16) -> io::Result<TcpListener> {
        let sock = self.socket(v6, Type::STREAM, port)?;
        sock.listen(128)?;
        TcpListener::from_std(sock.into())
    }

    pub async fn connect(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        // Reject mapped IPv6 addresses: they must never bypass IPv4 policy.
        if matches!(addr.ip(), IpAddr::V6(ip) if ip.to_ipv4_mapped().is_some()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPv4 mapeada no admitida",
            ));
        }
        let stream: std::net::TcpStream = self.socket(addr.is_ipv6(), Type::STREAM, 0)?.into();
        TcpSocket::from_std_stream(stream).connect(addr).await
    }

    pub async fn resolve(&self, target: &str) -> io::Result<Vec<SocketAddr>> {
        if !self.ready() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "Interfaz no disponible",
            ));
        }
        if let Ok(addr) = target.parse::<SocketAddr>() {
            self.local(addr.is_ipv6(), 0)?;
            return Ok(vec![addr]);
        }
        if self.config.interface.is_empty() {
            return Ok(lookup_host(target).await?.collect());
        }
        let (host, port) = target
            .rsplit_once(':')
            .ok_or_else(|| io::Error::other("Falta puerto"))?;
        let port: u16 = port.parse().map_err(io::Error::other)?;
        let name = Name::from_ascii(host).map_err(io::Error::other)?;
        let mut addresses = Vec::new();
        for (v6, kind) in [(false, RecordType::A), (true, RecordType::AAAA)] {
            if self.local(v6, 0).is_err() {
                continue;
            }
            let mut query = Message::new();
            query
                .set_id(rand::random())
                .set_message_type(MessageType::Query)
                .set_recursion_desired(true)
                .add_query(Query::query(name.clone(), kind));
            // No system DNS fallback, including on timeout or truncated replies.
            let sock = self.udp(self.config.dns.is_ipv6())?;
            sock.connect(SocketAddr::new(self.config.dns, 53)).await?;
            sock.send(&query.to_vec().map_err(io::Error::other)?)
                .await?;
            let mut buf = [0u8; 4096];
            let n = timeout(Duration::from_secs(4), sock.recv(&mut buf)).await??;
            let reply = Message::from_vec(&buf[..n]).map_err(io::Error::other)?;
            addresses.extend(dns_answers(&query, &reply, port)?);
        }
        if addresses.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "DNS sin direcciones compatibles",
            ));
        }
        Ok(addresses)
    }
}

fn dns_answers(query: &Message, reply: &Message, port: u16) -> io::Result<Vec<SocketAddr>> {
    if reply.id() != query.id()
        || reply.message_type() != MessageType::Response
        || reply.truncated()
        || reply.queries() != query.queries()
        || reply.response_code() != ResponseCode::NoError
    {
        return Err(io::Error::other("Respuesta DNS inválida o truncada"));
    }
    Ok(reply
        .answers()
        .iter()
        .filter_map(|record| match record.data() {
            RData::A(ip) => Some(SocketAddr::new(IpAddr::V4(ip.0), port)),
            RData::AAAA(ip) => Some(SocketAddr::new(IpAddr::V6(ip.0), port)),
            _ => None,
        })
        .collect())
}

#[cfg(windows)]
fn pin(sock: &Socket, adapter: &Adapter, v6: bool) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{
        setsockopt, WSAGetLastError, IPPROTO_IP, IPPROTO_IPV6, IPV6_UNICAST_IF, IP_UNICAST_IF,
        SOCKET_ERROR,
    };
    let (level, option, index) = if v6 {
        (IPPROTO_IPV6, IPV6_UNICAST_IF, adapter.index)
    } else {
        (IPPROTO_IP, IP_UNICAST_IF, adapter.index.to_be())
    };
    // Windows requires network byte order for IPv4 and host byte order for IPv6.
    let result = unsafe {
        setsockopt(
            sock.as_raw_socket() as _,
            level,
            option,
            (&index as *const u32).cast(),
            4,
        )
    };
    if result == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn pin(sock: &Socket, adapter: &Adapter, _v6: bool) -> io::Result<()> {
    sock.bind_device(Some(adapter.name.as_bytes()))
}

#[cfg(target_os = "macos")]
fn pin(sock: &Socket, adapter: &Adapter, v6: bool) -> io::Result<()> {
    let index = std::num::NonZeroU32::new(adapter.index)
        .ok_or_else(|| io::Error::other("Índice de interfaz inválido"))?;
    if v6 {
        sock.bind_device_by_index_v6(Some(index))
    } else {
        sock.bind_device_by_index_v4(Some(index))
    }
}

pub struct Network {
    config: Mutex<Config>,
    routes: watch::Sender<Arc<Route>>,
}

impl Network {
    pub fn new(config: Config) -> Arc<Self> {
        let (routes, _) = watch::channel(Arc::new(Route::discover(config.clone())));
        Arc::new(Self {
            config: Mutex::new(config),
            routes,
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<Route>> {
        self.routes.subscribe()
    }

    pub fn current(&self) -> Arc<Route> {
        self.routes.borrow().clone()
    }

    pub fn configure(&self, config: Config) -> io::Result<()> {
        if config.interface.len() > 256 || config.interface.contains('\0') || !usable(&config.dns) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Configuración de red inválida",
            ));
        }
        let mut saved = self.config.lock().unwrap();
        *saved = config;
        self.publish(&saved);
        Ok(())
    }

    fn publish(&self, config: &Config) {
        let route = Arc::new(Route::discover(config.clone()));
        self.routes.send_if_modified(|old| {
            if **old == *route {
                return false;
            }
            *old = route;
            true
        });
    }

    pub async fn monitor(self: Arc<Self>) {
        loop {
            sleep(Duration::from_secs(1)).await;
            let config = self.config.lock().unwrap();
            self.publish(&config);
        }
    }
}

/// Dropping a parent task must also close the detached peer writer / DHT receiver.
pub struct AbortOnDrop(pub tokio::task::JoinHandle<()>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::rr::{rdata::A, Record};

    fn missing_config() -> Config {
        Config {
            interface: "huillin-test-missing-interface".into(),
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn absent_interface_blocks_every_transport_and_dns() {
        let route = Route::discover(missing_config());
        assert!(!route.ready());
        for v6 in [false, true] {
            assert!(route.udp(v6).is_err());
            assert!(route.listen(v6, 0).is_err());
        }
        for target in ["127.0.0.1:80", "[::1]:80", "example.invalid:80"] {
            assert!(route.resolve(target).await.is_err());
        }
        assert!(route
            .connect("127.0.0.1:80".parse().unwrap())
            .await
            .is_err());
        assert!(route.connect("[::1]:80".parse().unwrap()).await.is_err());
    }

    #[tokio::test]
    async fn unavailable_address_family_never_uses_wildcard() {
        let route = Route {
            config: missing_config(),
            error: None,
            adapter: Some(Adapter {
                name: "test".into(),
                index: 1,
                addresses: vec!["192.0.2.1".parse().unwrap()],
            }),
        };
        assert_eq!(
            route.local(false, 42).unwrap(),
            "192.0.2.1:42".parse().unwrap()
        );
        assert!(route.local(true, 0).is_err());
        assert!(route.udp(true).is_err());
        assert!(route.listen(true, 0).is_err());
        assert!(route
            .connect("[::ffff:127.0.0.1]:80".parse().unwrap())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn route_changes_wake_workers_and_preserve_missing_selection() {
        let network = Network::new(Config::default());
        let mut routes = network.subscribe();
        network.configure(missing_config()).unwrap();
        routes.changed().await.unwrap();
        assert!(!routes.borrow_and_update().ready());
        assert_eq!(
            network.current().config.interface,
            missing_config().interface
        );
        network.configure(missing_config()).unwrap();
        assert!(!routes.has_changed().unwrap());
        network.configure(Config::default()).unwrap();
        routes.changed().await.unwrap();
        assert!(routes.borrow().ready());
    }

    #[tokio::test]
    async fn invalid_settings_do_not_replace_active_policy() {
        let network = Network::new(missing_config());
        let invalid = Config {
            dns: "0.0.0.0".parse().unwrap(),
            ..Config::default()
        };
        assert!(network.configure(invalid).is_err());
        assert_eq!(network.current().config, missing_config());
    }

    #[tokio::test]
    async fn unrestricted_tcp_and_udp_still_work() {
        let route = Route::discover(Config::default());
        let listener = route.listen(false, 0).unwrap();
        let addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            listener.local_addr().unwrap().port(),
        );
        let (connected, accepted) = tokio::join!(route.connect(addr), listener.accept());
        assert!(connected.is_ok());
        assert!(accepted.is_ok());
        let socket = route.udp(false).unwrap();
        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket
            .send_to(b"test", receiver.local_addr().unwrap())
            .await
            .unwrap();
        let mut bytes = [0; 8];
        assert_eq!(
            timeout(Duration::from_secs(1), receiver.recv(&mut bytes))
                .await
                .unwrap()
                .unwrap(),
            4
        );
    }

    #[test]
    fn dns_rejects_mismatched_and_truncated_responses() {
        let mut query = Message::new();
        let name = Name::from_ascii("example.test.").unwrap();
        query
            .set_id(42)
            .add_query(Query::query(name.clone(), RecordType::A));
        let mut reply = Message::new();
        reply
            .set_id(42)
            .set_message_type(MessageType::Response)
            .add_query(Query::query(name, RecordType::A))
            .add_answer(Record::from_rdata(
                Name::from_ascii("example.test.").unwrap(),
                30,
                RData::A(A(Ipv4Addr::new(192, 0, 2, 1))),
            ));
        assert_eq!(
            dns_answers(&query, &reply, 80).unwrap(),
            vec!["192.0.2.1:80".parse::<SocketAddr>().unwrap()]
        );
        reply.set_truncated(true);
        assert!(dns_answers(&query, &reply, 80).is_err());
        reply.set_truncated(false).set_id(43);
        assert!(dns_answers(&query, &reply, 80).is_err());
        reply.set_id(42).queries_mut().clear();
        assert!(dns_answers(&query, &reply, 80).is_err());
    }

    #[test]
    fn old_settings_default_to_unrestricted_and_selection_round_trips() {
        assert_eq!(
            serde_json::from_str::<Config>("{}").unwrap(),
            Config::default()
        );
        let config = missing_config();
        let saved = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&saved).unwrap(), config);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn selected_interface_sets_os_routing_and_source_address() {
        use std::os::windows::io::AsRawSocket;
        use windows_sys::Win32::Networking::WinSock::{getsockopt, IPPROTO_IP, IP_UNICAST_IF};
        let Some(adapter) = interfaces()
            .unwrap()
            .into_iter()
            .find(|a| a.addresses.iter().any(IpAddr::is_ipv4))
        else {
            return; // Offline hosts have no usable adapter to inspect.
        };
        let route = Route::discover(Config {
            interface: adapter.name.clone(),
            ..Config::default()
        });
        assert!(route.ready(), "{:?}", route.error);
        for kind in [Type::STREAM, Type::DGRAM] {
            let sock = route.socket(false, kind, 0).unwrap();
            assert!(adapter
                .addresses
                .contains(&sock.local_addr().unwrap().as_socket().unwrap().ip()));
            let mut index = 0u32;
            let mut len = 4;
            let result = unsafe {
                getsockopt(
                    sock.as_raw_socket() as _,
                    IPPROTO_IP,
                    IP_UNICAST_IF,
                    (&mut index as *mut u32).cast(),
                    &mut len,
                )
            };
            assert_eq!(result, 0);
            assert_eq!(index, adapter.index);
        }
    }
}
