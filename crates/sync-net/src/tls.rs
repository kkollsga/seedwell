//! TLS 1.3 between devices, authenticated by the device keys themselves (SPEC §7G.1).
//!
//! Each side presents its Ed25519 public key as an RFC 7250 raw public key; there are no certificates and no
//! certificate authorities. The handshake proves each side holds the private key of the key it shows; *which*
//! keys are welcome is decided afterwards, by the folder proofs (`Join`). So the verifiers here accept any
//! Ed25519 key and check only the handshake signatures.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, ring as provider};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, Error, ServerConfig, SignatureScheme, version,
};
use trs_sync_proto::device::{self, DeviceKey};

/// The ALPN protocol name: a TLS endpoint that speaks something else fails the handshake cleanly.
pub const ALPN: &[u8] = b"seedwell-sync/1";

/// The server name the client sends. Never resolved or checked: the peer is identified by its key.
const SERVER_NAME: &str = "seedwell-sync.invalid";

/// DER prefix of an Ed25519 SubjectPublicKeyInfo (RFC 8410): SEQUENCE { SEQUENCE { OID 1.3.101.112 },
/// BIT STRING (32 bytes) }.
const SPKI_PREFIX: [u8; 12] = [0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00];

/// The SubjectPublicKeyInfo of an Ed25519 key.
pub fn spki(pk: &[u8; 32]) -> Vec<u8> {
    [&SPKI_PREFIX[..], pk].concat()
}

/// The Ed25519 key in a SubjectPublicKeyInfo, `None` for anything else.
pub fn key_of_spki(der: &[u8]) -> Option<[u8; 32]> {
    let rest = der.strip_prefix(&SPKI_PREFIX[..])?;
    rest.try_into().ok()
}

/// Accepts any Ed25519 raw public key and checks TLS 1.3 handshake signatures against it.
#[derive(Debug)]
struct AnyDeviceKey;

impl AnyDeviceKey {
    fn check_key(cert: &CertificateDer<'_>) -> Result<[u8; 32], Error> {
        key_of_spki(cert.as_ref()).ok_or(Error::InvalidCertificate(rustls::CertificateError::BadEncoding))
    }

    fn check_signature(message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<(), Error> {
        if dss.scheme != SignatureScheme::ED25519 {
            return Err(Error::PeerIncompatible(rustls::PeerIncompatible::NoSignatureSchemesInCommon));
        }
        let pk = Self::check_key(cert)?;
        let sig: &[u8; 64] = dss
            .signature()
            .try_into()
            .map_err(|_| Error::InvalidCertificate(rustls::CertificateError::BadSignature))?;
        if device::verify(&pk, message, sig) {
            Ok(())
        } else {
            Err(Error::InvalidCertificate(rustls::CertificateError::BadSignature))
        }
    }
}

impl ServerCertVerifier for AnyDeviceKey {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Self::check_key(end_entity).map(|_| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("only TLS 1.3 is used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Self::check_signature(message, cert, dss).map(|()| HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl ClientCertVerifier for AnyDeviceKey {
    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        Self::check_key(end_entity).map(|_| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::General("only TLS 1.3 is used".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        Self::check_signature(message, cert, dss).map(|()| HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// The client and server configurations for this device.
#[derive(Clone)]
pub struct Tls {
    /// For connections we open.
    pub client: tokio_rustls::TlsConnector,
    /// For connections we accept.
    pub server: tokio_rustls::TlsAcceptor,
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(provider::default_provider())
}

impl Tls {
    /// TLS 1.3 only, Ed25519 raw public keys only, ALPN [`ALPN`].
    pub fn new(key: &DeviceKey) -> Result<Self, Error> {
        let pkcs8 = PrivatePkcs8KeyDer::from(key.to_pkcs8());
        let signing = provider::sign::any_eddsa_type(&pkcs8)?;
        let certified = Arc::new(CertifiedKey::new(vec![CertificateDer::from(spki(key.public_key()))], signing));

        let mut client = ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyDeviceKey))
            .with_client_cert_resolver(Arc::new(rustls::client::AlwaysResolvesClientRawPublicKeys::new(
                certified.clone(),
            )));
        client.alpn_protocols = vec![ALPN.to_vec()];
        client.resumption = rustls::client::Resumption::disabled();

        let mut server = ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&version::TLS13])?
            .with_client_cert_verifier(Arc::new(AnyDeviceKey))
            .with_cert_resolver(Arc::new(rustls::server::AlwaysResolvesServerRawPublicKeys::new(certified)));
        server.alpn_protocols = vec![ALPN.to_vec()];
        server.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        server.send_tls13_tickets = 0;

        Ok(Tls {
            client: tokio_rustls::TlsConnector::from(Arc::new(client)),
            server: tokio_rustls::TlsAcceptor::from(Arc::new(server)),
        })
    }

    /// The server name a client connection uses.
    pub fn server_name() -> ServerName<'static> {
        ServerName::try_from(SERVER_NAME).unwrap_or_else(|_| ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into()))
    }
}

/// What a finished handshake tells us: the peer's key and the value proofs are bound to.
#[derive(Clone, Copy, Debug)]
pub struct Established {
    /// The peer's Ed25519 public key.
    pub peer_pk: [u8; 32],
    /// The TLS exporter value for this session ([`trs_sync_proto::keys::EXPORTER_LABEL`]).
    pub exporter: [u8; 32],
}

/// Reads the peer key and exporter from a finished connection.
pub fn established<D>(conn: &rustls::ConnectionCommon<D>) -> Result<Established, Error> {
    let cert = conn
        .peer_certificates()
        .and_then(|c| c.first())
        .ok_or(Error::NoCertificatesPresented)?;
    let peer_pk = key_of_spki(cert.as_ref()).ok_or(Error::InvalidCertificate(rustls::CertificateError::BadEncoding))?;
    if conn.alpn_protocol() != Some(ALPN) {
        return Err(Error::NoApplicationProtocol);
    }
    let mut exporter = [0u8; 32];
    conn.export_keying_material(&mut exporter, trs_sync_proto::keys::EXPORTER_LABEL.as_bytes(), None)?;
    Ok(Established { peer_pk, exporter })
}
