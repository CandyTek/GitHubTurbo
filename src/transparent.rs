use super::*;
use hickory_proto::{
    op::{Message, MessageType, ResponseCode},
    rr::{rdata::A, RData, Record, RecordType},
};
use std::{net::Ipv4Addr, path::PathBuf};
use tokio::net::UdpSocket;
use tokio_rustls::{rustls, TlsAcceptor};

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub https_listen: SocketAddr,
    pub http_listen: SocketAddr,
    pub dns_listen: SocketAddr,
    pub state_dir: PathBuf,
    pub try_without_sni: bool,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            https_listen: "127.0.0.1:443".parse().unwrap(),
            http_listen: "127.0.0.1:80".parse().unwrap(),
            dns_listen: "127.0.0.1:53".parse().unwrap(),
            state_dir: "state".into(),
            try_without_sni: true,
        }
    }
}
impl Config {
    pub fn validate(&self, listen: SocketAddr) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }
        let mut ports = vec![listen.port()];
        for address in [self.https_listen, self.http_listen, self.dns_listen] {
            if !address.ip().is_loopback() || address.port() == 0 || ports.contains(&address.port())
            {
                return Err(err(
                    "transparent listeners must use distinct loopback ports",
                ));
            }
            ports.push(address.port());
        }
        Ok(())
    }
}
// Covers the service families we accept, with no certificate for unrelated sites.
fn names() -> Vec<String> {
    let mut names: Vec<_> = DOMAINS
        .iter()
        .flat_map(|name| [name.to_string(), format!("*.{name}")])
        .collect();
    names.push("*.actions.githubusercontent.com".into());
    names
}
pub fn init_ca(config: &Config, control: SocketAddr) -> Result<()> {
    use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, KeyPair, KeyUsagePurpose};
    let dir = &config.state_dir;
    std::fs::create_dir_all(dir)?;
    let cert_path = dir.join("ca.pem");
    let key_path = dir.join("ca-key.pem");
    let (ca, key) = if cert_path.exists() && key_path.exists() {
        let key = KeyPair::from_pem(&std::fs::read_to_string(&key_path)?)?;
        let params = CertificateParams::from_ca_cert_pem(&std::fs::read_to_string(&cert_path)?)?;
        (params.self_signed(&key)?, key)
    } else {
        if cert_path.exists() || key_path.exists() {
            return Err(err(
                "incomplete CA state; restore before generating a new CA",
            ));
        }
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "GitHubTurbo Local CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(3650);
        let ca = params.self_signed(&key)?;
        // State directory ACL is restricted by the Windows launcher before this command.
        std::fs::write(&key_path, key.serialize_pem())?;
        std::fs::write(&cert_path, ca.pem())?;
        (ca, key)
    };
    let leaf_key = KeyPair::generate()?;
    let mut params = CertificateParams::new(names())?;
    params
        .distinguished_name
        .push(DnType::CommonName, "GitHubTurbo local GitHub bridge");
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::days(1);
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(90);
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.crl_distribution_points = vec![rcgen::CrlDistributionPoint {
        uris: vec![format!("http://{control}/ca.crl")],
    }];
    let now = time::OffsetDateTime::now_utc();
    let crl = rcgen::CertificateRevocationListParams {
        this_update: now - time::Duration::hours(1),
        next_update: now + time::Duration::days(91),
        crl_number: (now.unix_timestamp() as u64).into(),
        issuing_distribution_point: None,
        revoked_certs: vec![],
        key_identifier_method: rcgen::KeyIdMethod::Sha256,
    }
    .signed_by(&ca, &key)?;
    std::fs::write(dir.join("ca.crl"), crl.der())?;
    let leaf = params.signed_by(&leaf_key, &ca, &key)?;
    std::fs::write(
        dir.join("server.pem"),
        format!("{}{}", leaf.pem(), std::fs::read_to_string(&cert_path)?),
    )?;
    std::fs::write(dir.join("server-key.pem"), leaf_key.serialize_pem())?;
    println!("{}", cert_path.display());
    Ok(())
}
pub(crate) fn acceptor(config: &Config) -> Result<TlsAcceptor> {
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(std::fs::File::open(
        config.state_dir.join("server.pem"),
    )?))
    .collect::<std::result::Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(std::fs::File::open(
        config.state_dir.join("server-key.pem"),
    )?))?
    .ok_or_else(|| err("missing server key"))?;
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    // Bridge carries HTTP/1.1 unchanged, including upgrades, Range, cookies and Git POSTs.
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsAcceptor::from(Arc::new(tls)))
}
pub(crate) async fn bridge(
    stream: TcpStream,
    state: Arc<State>,
    accept: TlsAcceptor,
) -> Result<()> {
    let client = timeout(Duration::from_secs(10), accept.accept(stream)).await??;
    let host = client
        .get_ref()
        .1
        .server_name()
        .ok_or_else(|| err("client SNI is required"))?
        .to_ascii_lowercase();
    target(&format!("{host}:443"))?;
    crate::forward::serve(client, state, host).await
}
pub fn dns_reply(packet: &[u8]) -> Result<Vec<u8>> {
    let query = Message::from_vec(packet)?;
    let mut reply = Message::new();
    reply
        .set_id(query.id())
        .set_message_type(MessageType::Response)
        .set_recursion_desired(query.recursion_desired())
        .set_recursion_available(false);
    if query.message_type() != MessageType::Query || query.queries().len() != 1 {
        reply.set_response_code(ResponseCode::FormErr);
        return Ok(reply.to_vec()?);
    }
    let q = &query.queries()[0];
    reply.add_query(q.clone());
    let host = q
        .name()
        .to_ascii()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !allowed(&host) {
        reply.set_response_code(ResponseCode::Refused);
        return Ok(reply.to_vec()?);
    }
    reply.set_authoritative(true);
    // Publish IPv4 loopback only: listeners are IPv4. Do not advertise an unbound ::1 endpoint.
    let data = match q.query_type() {
        RecordType::A => Some(RData::A(A(Ipv4Addr::LOCALHOST))),
        _ => None,
    };
    if let Some(data) = data {
        reply.add_answer(Record::from_rdata(q.name().clone(), 15, data));
    }
    Ok(reply.to_vec()?)
}
async fn dns_udp(socket: UdpSocket) -> Result<()> {
    let mut buffer = [0u8; 4096];
    loop {
        let (len, peer) = socket.recv_from(&mut buffer).await?;
        if let Ok(reply) = dns_reply(&buffer[..len]) {
            socket.send_to(&reply, peer).await?;
        }
    }
}
async fn dns_tcp(mut stream: TcpStream) -> Result<()> {
    timeout(Duration::from_secs(5), async {
        for _ in 0..16 {
            let len = stream.read_u16().await? as usize;
            if len > 4096 {
                return Err(err("oversized DNS request"));
            }
            let mut packet = vec![0; len];
            stream.read_exact(&mut packet).await?;
            let reply = dns_reply(&packet)?;
            stream.write_u16(reply.len() as u16).await?;
            stream.write_all(&reply).await?;
        }
        Ok(())
    })
    .await?
}
async fn http_redirect(mut stream: TcpStream) -> Result<()> {
    let header = timeout(Duration::from_secs(5), read_header(&mut stream)).await??;
    let text = std::str::from_utf8(&header)?;
    let host = text
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("host"))
                .map(|(_, value)| value.trim())
        })
        .ok_or_else(|| err("missing Host"))?;
    let host = host
        .strip_suffix(":80")
        .unwrap_or(host)
        .to_ascii_lowercase();
    target(&format!("{host}:443"))?;
    let path = text
        .lines()
        .next()
        .and_then(|line| line.split_ascii_whitespace().nth(1))
        .filter(|p| p.starts_with('/') && !p.starts_with("//"))
        .ok_or_else(|| err("invalid path"))?;
    stream.write_all(format!("HTTP/1.1 308 Permanent Redirect\r\nLocation: https://{host}{path}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await?;
    Ok(())
}
pub async fn ssh_stdio(state: Arc<State>) -> Result<()> {
    let mut remote = None;
    if let Ok(addresses) = state.addresses("ssh.github.com").await {
        for address in addresses {
            if let Ok(Ok(stream)) =
                timeout(Duration::from_secs(2), TcpStream::connect(address)).await
            {
                remote = Some(stream);
                break;
            }
        }
    }
    if remote.is_none() && state.config.upstream.is_some() {
        remote = Some(state.dial("ssh.github.com", &Route::Proxy).await?);
    }
    let mut remote = remote.ok_or_else(|| err("SSH endpoint unavailable"))?;
    let (mut reader, mut writer) = remote.split();
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    let upload = async {
        tokio::io::copy(&mut input, &mut writer).await?;
        writer.shutdown().await
    };
    let download = async {
        tokio::io::copy(&mut reader, &mut output).await?;
        output.flush().await
    };
    // Remote EOF must end ProxyCommand even while the blocking stdin read is waiting.
    tokio::select! {result=upload=>{result?;}, result=download=>{result?;}}
    Ok(())
}
pub struct Listeners {
    https: TcpListener,
    http: TcpListener,
    dns: TcpListener,
    udp: UdpSocket,
    accept: TlsAcceptor,
}
impl Listeners {
    pub async fn bind(config: &Config) -> Result<Self> {
        let accept = acceptor(config)?;
        Ok(Self {
            https: TcpListener::bind(config.https_listen).await?,
            http: TcpListener::bind(config.http_listen).await?,
            dns: TcpListener::bind(config.dns_listen).await?,
            udp: UdpSocket::bind(config.dns_listen).await?,
            accept,
        })
    }
    pub async fn run(self, state: Arc<State>) -> Result<()> {
        let Self {
            https,
            http,
            dns,
            udp,
            accept,
        } = self;
        let slots = Arc::new(Semaphore::new(state.config.max_connections));
        let mut jobs: JoinSet<Result<()>> = JoinSet::new();
        let mut udp_task = tokio::spawn(async move { dns_udp(udp).await });
        loop {
            let (stream, kind) = tokio::select! {
                value=https.accept()=>(value?.0,0),value=http.accept()=>(value?.0,1),value=dns.accept()=>(value?.0,2),
                result=&mut udp_task=>{return Err(err(&format!("DNS UDP listener stopped: {result:?}")));},
                Some(result)=jobs.join_next()=>{if let Ok(Err(error))=result{eprintln!("transparent: {error}");}continue;}
            };
            let permit = match slots.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let state = state.clone();
            let accept = accept.clone();
            state.counters.accepted.fetch_add(1, Ordering::Relaxed);
            jobs.spawn(async move {
                let _permit = permit;
                let result = match kind {
                    0 => bridge(stream, state.clone(), accept).await,
                    1 => http_redirect(stream).await,
                    _ => dns_tcp(stream).await,
                };
                if let Err(error) = result {
                    state.counters.failures.fetch_add(1, Ordering::Relaxed);
                    eprintln!("transparent connection: {error}");
                }
                state.counters.completed.fetch_add(1, Ordering::Relaxed);
                Ok(())
            });
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use hickory_proto::op::Query;
    use hickory_proto::rr::Name;
    #[test]
    fn dns_scope_and_record_types() {
        for (host, kind, answers, code) in [
            (
                "raw.githubusercontent.com",
                RecordType::A,
                1,
                ResponseCode::NoError,
            ),
            ("foo.github.io", RecordType::AAAA, 0, ResponseCode::NoError),
            ("github.com", RecordType::HTTPS, 0, ResponseCode::NoError),
            (
                "github.com.evil.test",
                RecordType::A,
                0,
                ResponseCode::Refused,
            ),
        ] {
            let mut query = Message::new();
            query
                .set_id(123)
                .add_query(Query::query(Name::from_ascii(host).unwrap(), kind));
            let reply = Message::from_vec(&dns_reply(&query.to_vec().unwrap()).unwrap()).unwrap();
            assert_eq!(reply.id(), 123);
            assert_eq!(reply.answers().len(), answers);
            assert_eq!(reply.response_code(), code);
        }
    }
}
