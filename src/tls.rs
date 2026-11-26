//! TLS on qh-link's TCP lane to a relay, so the lane looks like HTTPS on 443 and passes networks
//! that inspect traffic as well as ports. It adds no trust: every packet inside is already sealed
//! end to end and proven to the relay, so the client takes any certificate and the relay makes a
//! throwaway self-signed one at start.

use std::io::Read;
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring::default_provider, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{Connection, DigitallySignedStruct, SignatureScheme};

/// How long a client gives the relay's TLS handshake, start to finish.
const CONNECT_WITHIN: Duration = Duration::from_secs(15);

/// One TCP connection carrying framed packets, plain or inside TLS. `send` may be called from any
/// thread while one reader thread calls `recv`. Both share one socket (one file descriptor).
pub struct Wire {
    w: Mutex<(Arc<TcpStream>, Option<Connection>)>,
}

pub struct Reader {
    wire: Arc<Wire>,
    r: Arc<TcpStream>,
    plain: Vec<u8>,
}

fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| Arc::new(default_provider())).clone()
}

fn err(e: rustls::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}

/// What is left until `deadline`, or a timeout error once it has passed.
fn left(deadline: Instant) -> std::io::Result<Duration> {
    deadline.checked_duration_since(Instant::now()).filter(|d| *d >= Duration::from_millis(1))
        .ok_or_else(|| std::io::ErrorKind::TimedOut.into())
}

/// The handshake must finish by `deadline` as a whole, not just per read, so a peer that trickles
/// a byte at a time can't hold the connection open. The socket's own timeouts are put back after.
fn handshake(s: TcpStream, mut c: Connection, deadline: Instant) -> std::io::Result<(Arc<Wire>, Reader)> {
    let (rt, wt) = (s.read_timeout()?, s.write_timeout()?);
    let s = Arc::new(s);
    let mut io = &*s;
    while c.is_handshaking() || c.wants_write() {
        let l = left(deadline)?;
        s.set_read_timeout(Some(rt.map_or(l, |t| t.min(l))))?;
        s.set_write_timeout(Some(wt.map_or(l, |t| t.min(l))))?;
        if c.wants_write() {
            c.write_tls(&mut io)?;
            continue;
        }
        if c.read_tls(&mut io)? == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        if let Err(e) = c.process_new_packets() {
            let _ = c.write_tls(&mut io); // the alert, if it goes
            return Err(err(e));
        }
    }
    s.set_read_timeout(rt)?;
    s.set_write_timeout(wt)?;
    // packets that came in with the last handshake read are already decrypted: keep them
    let mut plain = vec![];
    let mut buf = [0u8; 4096];
    while let Ok(n @ 1..) = c.reader().read(&mut buf) {
        plain.extend_from_slice(&buf[..n]);
    }
    Ok(split(s, Some(c), plain))
}

fn split(s: Arc<TcpStream>, c: Option<Connection>, plain: Vec<u8>) -> (Arc<Wire>, Reader) {
    let wire = Arc::new(Wire { w: Mutex::new((s.clone(), c)) });
    (wire.clone(), Reader { wire, r: s, plain })
}

/// The client end: TLS to the relay, any certificate taken.
pub fn connect(s: TcpStream) -> std::io::Result<(Arc<Wire>, Reader)> {
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let cfg = match CFG.get() {
        Some(c) => c.clone(),
        None => {
            let mut c = rustls::ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions().map_err(err)?
                .dangerous().with_custom_certificate_verifier(Arc::new(AnyCert))
                .with_no_client_auth();
            c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
            CFG.get_or_init(|| Arc::new(c)).clone()
        }
    };
    let ip = s.peer_addr()?.ip();
    let c = rustls::ClientConnection::new(cfg, ServerName::IpAddress(ip.into())).map_err(err)?;
    handshake(s, c.into(), Instant::now() + CONNECT_WITHIN)
}

/// The relay's TLS settings, made once (with a fresh certificate); an error rather than a panic if
/// that can't be done, so one bad moment costs a connection, not the relay.
fn server_config() -> std::io::Result<Arc<rustls::ServerConfig>> {
    static CFG: OnceLock<Arc<rustls::ServerConfig>> = OnceLock::new();
    if let Some(c) = CFG.get() {
        return Ok(c.clone());
    }
    let (cert, key) = self_signed().ok_or_else(|| std::io::Error::other("can't make the relay certificate"))?;
    let mut c = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions().map_err(err)?
        .with_no_client_auth()
        .with_single_cert(vec![cert], PrivateKeyDer::Pkcs8(key)).map_err(err)?;
    c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(CFG.get_or_init(|| Arc::new(c)).clone())
}

/// The relay end: TLS when the client opens with a TLS record, plain framing otherwise. All of it
/// (the first byte too) must be done by `deadline`.
pub fn accept(s: TcpStream, deadline: Instant) -> std::io::Result<(Arc<Wire>, Reader)> {
    let rt = s.read_timeout()?;
    let l = left(deadline)?;
    s.set_read_timeout(Some(rt.map_or(l, |t| t.min(l))))?;
    let mut b = [0u8; 1];
    let n = s.peek(&mut b)?;
    s.set_read_timeout(rt)?;
    if n == 0 {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    if b[0] != 0x16 {
        return Ok(split(Arc::new(s), None, vec![]));
    }
    let c = rustls::ServerConnection::new(server_config()?).map_err(err)?;
    handshake(s, c.into(), deadline)
}

impl Wire {
    pub fn send(&self, pkt: &[u8]) -> std::io::Result<()> {
        let mut w = self.w.lock().unwrap_or_else(|e| e.into_inner());
        let (s, c) = &mut *w;
        let mut io = &**s;
        match c {
            None => crate::link::write_frame(&mut io, pkt),
            Some(c) => {
                crate::link::write_frame(&mut c.writer(), pkt)?;
                while c.wants_write() {
                    c.write_tls(&mut io)?;
                }
                Ok(())
            }
        }
    }

    pub fn shutdown(&self) {
        let w = self.w.lock().unwrap_or_else(|e| e.into_inner());
        let _ = w.0.shutdown(std::net::Shutdown::Both);
    }
}

impl Reader {
    /// The next packet; an error when the connection is closed, broken or silent past its timeout.
    pub fn recv(&mut self) -> std::io::Result<Vec<u8>> {
        loop {
            if self.plain.len() >= 2 {
                let n = u16::from_be_bytes([self.plain[0], self.plain[1]]) as usize;
                if n > 2048 {
                    return Err(std::io::ErrorKind::InvalidData.into());
                }
                if self.plain.len() >= 2 + n {
                    let pkt = self.plain[2..2 + n].to_vec();
                    self.plain.drain(..2 + n);
                    return Ok(pkt);
                }
            }
            let mut buf = [0u8; 4096];
            let n = (&*self.r).read(&mut buf)?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            let mut w = self.wire.w.lock().unwrap_or_else(|e| e.into_inner());
            let (s, c) = &mut *w;
            let mut io = &**s;
            let Some(c) = c else {
                self.plain.extend_from_slice(&buf[..n]);
                continue;
            };
            let mut raw = &buf[..n];
            while !raw.is_empty() {
                c.read_tls(&mut raw)?;
                let st = c.process_new_packets().map_err(err)?;
                if st.peer_has_closed() {
                    return Err(std::io::ErrorKind::UnexpectedEof.into());
                }
                let k = st.plaintext_bytes_to_read();
                if k > 0 {
                    let at = self.plain.len();
                    self.plain.resize(at + k, 0);
                    c.reader().read_exact(&mut self.plain[at..])?;
                }
                if self.plain.len() > 2 + 2048 + 16384 {
                    return Err(std::io::ErrorKind::InvalidData.into());
                }
            }
            while c.wants_write() {
                c.write_tls(&mut io)?;
            }
        }
    }
}

/// Takes any certificate but still checks the handshake signature against it, as TLS needs.
#[derive(Debug)]
struct AnyCert;

impl ServerCertVerifier for AnyCert {
    fn verify_server_cert(&self, _: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime)
        -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, m: &[u8], cert: &CertificateDer<'_>, d: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, cert, d, &provider().signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], cert: &CertificateDer<'_>, d: &DigitallySignedStruct)
        -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, cert, d, &provider().signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider().signature_verification_algorithms.supported_schemes()
    }
}

fn der(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut v = vec![tag];
    match body.len() {
        n if n < 0x80 => v.push(n as u8),
        n if n < 0x100 => v.extend([0x81, n as u8]),
        n => v.extend([0x82, (n >> 8) as u8, n as u8]),
    }
    v.extend_from_slice(body);
    v
}

/// A fresh Ed25519 key and a self-signed X.509 certificate for it, built by hand with ring.
fn self_signed() -> Option<(CertificateDer<'static>, PrivatePkcs8KeyDer<'static>)> {
    use ring::signature::{Ed25519KeyPair, KeyPair};
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).ok()?;
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).ok()?;
    let alg = der(0x30, &der(0x06, &[0x2b, 0x65, 0x70])); // id-Ed25519
    let cn = der(0x0c, b"relay");
    let name = der(0x30, &der(0x31, &der(0x30, &[der(0x06, &[0x55, 0x04, 0x03]), cn].concat())));
    let validity = der(0x30, &[der(0x17, b"250101000000Z"), der(0x18, b"99991231235959Z")].concat());
    let mut serial = crate::oauth::random()[..8].to_vec();
    serial[0] &= 0x7f;
    serial[0] |= 0x01;
    let spki = der(0x30, &[alg.clone(), der(0x03, &[&[0u8][..], kp.public_key().as_ref()].concat())].concat());
    let tbs = der(0x30, &[
        der(0xa0, &der(0x02, &[2])), der(0x02, &serial), alg.clone(), name.clone(), validity, name, spki,
    ].concat());
    let sig = kp.sign(&tbs);
    let cert = der(0x30, &[tbs, alg, der(0x03, &[&[0u8][..], sig.as_ref()].concat())].concat());
    Some((CertificateDer::from(cert), PrivatePkcs8KeyDer::from(pkcs8.as_ref().to_vec())))
}
