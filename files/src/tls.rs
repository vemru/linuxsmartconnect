//! Mutual TLS to the phone's NFS ports, with the peer pinned by certificate fingerprint.
//!
//! The phone verifies our client certificate by the SHA-256 fingerprint we announced in the
//! `cmd=1001` handshake (not a CA chain), and we verify the phone the same way: against the `fp` it
//! returned. So the rustls config uses a custom verifier that checks only the fingerprint, plus our
//! self-signed client certificate for the mutual-auth half. See `docs/protocol/file-exchange.md`.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio::net::{TcpSocket, TcpStream};
use tokio_rustls::TlsConnector;

use crate::cert::{Identity, fingerprint_of};
use nfs3_client::net::Connector;
use nfs3_client::tokio::TokioIo;

/// Verifies the phone's TLS certificate by its SHA-256 fingerprint only (no CA chain, no hostname).
#[derive(Debug)]
struct FingerprintVerifier {
    /// Uppercase-hex SHA-256 of the phone's certificate DER, from the handshake reply.
    expected: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for FingerprintVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = fingerprint_of(end_entity.as_ref());
        if got.eq_ignore_ascii_case(&self.expected) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "phone certificate fingerprint did not match the one from the handshake".into(),
            ))
        }
    }

    // The phone presents a minimal self-signed certificate (observed: X.509 v1), which webpki
    // rejects outright (`UnsupportedCertVersion`) — so we cannot route the handshake-signature check
    // through `rustls::crypto::verify_tls1x_signature`, which parses the cert with webpki. We instead
    // verify the signature directly against the public key we extract from the (already
    // fingerprint-pinned) certificate. The pin guarantees the cert bytes are exactly the ones the
    // phone announced over the authenticated control channel; this step then proves the peer holds
    // the matching private key, so a party that merely learned the fingerprint cannot impersonate it.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_signature(cert, message, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_signature(cert, message, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Verifies a TLS handshake signature against the public key in `cert`, parsing the key ourselves
/// (webpki rejects the phone's X.509 v1 cert). The cert is already fingerprint-pinned by the caller.
///
/// Shared by the client [`FingerprintVerifier`] (above) and the server-side client-cert verifier in
/// [`crate::server`]: both pin the phone by `SHA-256(DER)` and then must prove it holds the matching
/// private key without routing through webpki.
pub(crate) fn verify_signature(
    cert: &CertificateDer<'_>,
    message: &[u8],
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, rustls::Error> {
    use ring::signature;
    let bad = |m: &str| rustls::Error::General(m.to_string());
    let (_, parsed) = x509_parser::parse_x509_certificate(cert.as_ref())
        .map_err(|_| bad("could not parse the phone's certificate"))?;
    let key = parsed.public_key().subject_public_key.data.as_ref();
    let alg: &dyn signature::VerificationAlgorithm = match dss.scheme {
        SignatureScheme::RSA_PKCS1_SHA256 => &signature::RSA_PKCS1_2048_8192_SHA256,
        SignatureScheme::RSA_PKCS1_SHA384 => &signature::RSA_PKCS1_2048_8192_SHA384,
        SignatureScheme::RSA_PKCS1_SHA512 => &signature::RSA_PKCS1_2048_8192_SHA512,
        SignatureScheme::RSA_PSS_SHA256 => &signature::RSA_PSS_2048_8192_SHA256,
        SignatureScheme::RSA_PSS_SHA384 => &signature::RSA_PSS_2048_8192_SHA384,
        SignatureScheme::RSA_PSS_SHA512 => &signature::RSA_PSS_2048_8192_SHA512,
        SignatureScheme::ECDSA_NISTP256_SHA256 => &signature::ECDSA_P256_SHA256_ASN1,
        SignatureScheme::ECDSA_NISTP384_SHA384 => &signature::ECDSA_P384_SHA384_ASN1,
        SignatureScheme::ED25519 => &signature::ED25519,
        other => return Err(bad(&format!("unsupported signature scheme {other:?}"))),
    };
    signature::UnparsedPublicKey::new(alg, key)
        .verify(message, dss.signature())
        .map(|()| HandshakeSignatureValid::assertion())
        .map_err(|_| bad("the phone's handshake signature did not verify"))
}

/// Builds the rustls client config: present our cert, pin the phone by `phone_fingerprint`.
pub fn client_config(id: &Identity, phone_fingerprint: &str) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(FingerprintVerifier {
        expected: phone_fingerprint.to_string(),
        provider: provider.clone(),
    });
    let cert_chain = vec![CertificateDer::from(id.cert_der.clone())];
    let key = PrivatePkcs8KeyDer::from(id.key_der.clone());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("selecting TLS protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(cert_chain, key.into())
        .context("installing our client certificate")?;
    Ok(Arc::new(config))
}

/// An `nfs3_client` connector that opens a fingerprint-pinned mutual-TLS stream instead of plain TCP.
pub struct TlsNfsConnector {
    config: Arc<ClientConfig>,
    /// A placeholder server name; the fingerprint verifier ignores it.
    server_name: ServerName<'static>,
}

impl TlsNfsConnector {
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self {
            config,
            server_name: ServerName::try_from("smartconnect-phone").expect("static name"),
        }
    }

    async fn tls(
        &self,
        stream: TcpStream,
    ) -> std::io::Result<TokioIo<tokio_rustls::client::TlsStream<TcpStream>>> {
        let connector = TlsConnector::from(self.config.clone());
        let tls = connector.connect(self.server_name.clone(), stream).await?;
        Ok(TokioIo::new(tls))
    }
}

impl Connector for TlsNfsConnector {
    type Connection = TokioIo<tokio_rustls::client::TlsStream<TcpStream>>;

    async fn connect(&self, addr: SocketAddr) -> std::io::Result<Self::Connection> {
        let stream = TcpStream::connect(addr).await?;
        self.tls(stream).await
    }

    async fn connect_with_port(
        &self,
        addr: SocketAddr,
        local_port: u16,
    ) -> std::io::Result<Self::Connection> {
        let socket = TcpSocket::new_v4()?;
        socket.bind(SocketAddr::new(
            std::net::Ipv4Addr::UNSPECIFIED.into(),
            local_port,
        ))?;
        let stream = socket.connect(addr).await?;
        self.tls(stream).await
    }
}
