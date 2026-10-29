//! TLS on qh-link's TCP lane to a relay, so the lane looks like HTTPS on 443 and passes networks
//! that inspect traffic as well as ports. It adds no trust: every packet inside is already sealed
//! end to end and proven to the relay, so the client takes any certificate and the relay makes a
//! throwaway self-signed one at start.

use std::io::Read;
use std::net::TcpStream;
use std::sync::{Arc, Mutex, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{ring::default_provider, CryptoProvider};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{Connection, DigitallySignedStruct, SignatureScheme};

/// One TCP connection carrying framed packets, plain or inside TLS. `send` may be called from any
/// thread while one reader thread calls `recv`.
pub struct Wire {
    w: Mutex<(TcpStream, Option<Connection>)>,
}

pub struct Reader {
    wire: Arc<Wire>,
    r: TcpStream,
    plain: Vec<u8>,
}

fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    P.get_or_init(|| Arc::new(default_provider())).clone()
}

fn err(e: rustls::Error) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e)
}

fn handshake(s: &mut TcpStream, mut c: Connection) -> std::io::Result<(Arc<Wire>, Reader)> {
    while c.is_handshaking() {
        c.complete_io(s)?;
    }
    Ok(split(s, Some(c))?)
}

fn split(s: &TcpStream, c: Option<Connection>) -> std::io::Result<(Arc<Wire>, Reader)> {
    let wire = Arc::new(Wire { w: Mutex::new((s.try_clone()?, c)) });
    Ok((wire.clone(), Reader { wire, r: s.try_clone()?, plain: vec![] }))
}

/// The client end: TLS to the relay, any certificate taken.
pub fn connect(mut s: TcpStream) -> std::io::Result<(Arc<Wire>, Reader)> {
    static CFG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let cfg = CFG.get_or_init(|| {
        let mut c = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions().expect("tls versions")
            .dangerous().with_custom_certificate_verifier(Arc::new(AnyCert))
            .with_no_client_auth();
        c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Arc::new(c)
    });
    let ip = s.peer_addr()?.ip();
    let c = rustls::ClientConnection::new(cfg.clone(), ServerName::IpAddress(ip.into())).map_err(err)?;
    handshake(&mut s, c.into())
}

/// The relay end: TLS when the client opens with a TLS record, plain framing otherwise.
pub fn accept(mut s: TcpStream) -> std::io::Result<(Arc<Wire>, Reader)> {
    let mut b = [0u8; 1];
    if s.peek(&mut b)? == 0 {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    if b[0] != 0x16 {
        return split(&s, None);
    }
    static CFG: OnceLock<Arc<rustls::ServerConfig>> = OnceLock::new();
    let cfg = CFG.get_or_init(|| {
        let (cert, key) = self_signed();
        let mut c = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions().expect("tls versions")
            .with_no_client_auth()
            .with_single_cert(vec![cert], PrivateKeyDer::Pkcs8(key)).expect("relay certificate");
        c.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Arc::new(c)
    });
    let c = rustls::ServerConnection::new(cfg.clone()).map_err(err)?;
    handshake(&mut s, c.into())
}

impl Wire {
    pub fn send(&self, pkt: &[u8]) -> std::io::Result<()> {
        let mut w = self.w.lock().unwrap_or_else(|e| e.into_inner());
        let (s, c) = &mut *w;
        match c {
            None => crate::link::write_frame(s, pkt),
            Some(c) => {
                crate::link::write_frame(&mut c.writer(), pkt)?;
                while c.wants_write() {
                    c.write_tls(s)?;
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
            let n = self.r.read(&mut buf)?;
            if n == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            let mut w = self.wire.w.lock().unwrap_or_else(|e| e.into_inner());
            let (s, c) = &mut *w;
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
                c.write_tls(s)?;
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
fn self_signed() -> (CertificateDer<'static>, PrivatePkcs8KeyDer<'static>) {
    use ring::signature::{Ed25519KeyPair, KeyPair};
    let rng = ring::rand::SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("ed25519 key");
    let kp = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("ed25519 key");
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
    (CertificateDer::from(cert), PrivatePkcs8KeyDer::from(pkcs8.as_ref().to_vec()))
}
