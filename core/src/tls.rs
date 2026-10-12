//! TLS setup for the two places that need it, per backend:
//!
//! - the remote WebSocket (`wss://host:8002`): the TV presents a self-signed certificate,
//!   so any certificate and host name is accepted;
//! - HTTPS to SmartThings: normal certificate validation.
//!
//! Backends: `native-tls` (OpenSSL, the default; the desktop build) or `rustls` (pure Rust,
//! for Android, where OpenSSL is painful to cross-compile). With both features on, rustls
//! wins, so the desktop apps can try it against a real TV.

#[cfg(not(any(feature = "native-tls", feature = "rustls")))]
compile_error!("tvremote_core needs a TLS backend: enable the `native-tls` (default) or `rustls` feature");

use std::net::TcpStream;
use std::time::Duration;

use tungstenite::stream::MaybeTlsStream;

use crate::error::Result;

/// The name of the TLS backend compiled in (`"rustls"` or `"native-tls"`), for diagnostics.
pub fn backend() -> &'static str {
    if cfg!(feature = "rustls") {
        "rustls"
    } else {
        "native-tls"
    }
}

/// Connector for the TV's WebSocket: accepts invalid certificates and host names.
pub(crate) fn tv_ws_connector() -> Result<tungstenite::Connector> {
    imp::tv_ws_connector()
}

/// Add certificate-validating TLS to an agent (SmartThings).
pub(crate) fn verified(builder: ureq::AgentBuilder) -> Result<ureq::AgentBuilder> {
    imp::verified(builder)
}

/// Set the read timeout of the TCP socket under a (possibly TLS) stream.
pub(crate) fn set_read_timeout(s: &mut MaybeTlsStream<TcpStream>, d: Duration) {
    let tcp: Option<&mut TcpStream> = match s {
        MaybeTlsStream::Plain(s) => Some(s),
        #[cfg(feature = "native-tls")]
        MaybeTlsStream::NativeTls(s) => Some(s.get_mut()),
        #[cfg(feature = "rustls")]
        MaybeTlsStream::Rustls(s) => Some(&mut s.sock),
        _ => None,
    };
    if let Some(t) = tcp {
        let _ = t.set_read_timeout(Some(d));
    }
}

#[cfg(feature = "rustls")]
mod imp {
    use std::sync::{Arc, OnceLock};

    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::crypto::CryptoProvider;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

    use crate::error::{Error, Result};

    /// The ring provider, named explicitly: another crate in the build enabling
    /// aws-lc-rs must not make `ClientConfig::builder()` ambiguous (it panics then).
    fn provider() -> Arc<CryptoProvider> {
        Arc::new(rustls::crypto::ring::default_provider())
    }

    fn tls_err(e: rustls::Error) -> Error {
        Error::Http(format!("TLS init failed: {e}"))
    }

    /// Accepts any server certificate, like native-tls's `danger_accept_invalid_certs` +
    /// `danger_accept_invalid_hostnames`. Handshake signatures are not checked either:
    /// the key they prove is from an unauthenticated certificate, so checking adds no
    /// protection, and it would reject old TVs whose self-signed keys ring refuses
    /// (e.g. RSA under 2048 bits).
    #[derive(Debug)]
    struct AcceptAnyCert(Arc<CryptoProvider>);

    impl ServerCertVerifier for AcceptAnyCert {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> std::result::Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    pub(super) fn tv_ws_connector() -> Result<tungstenite::Connector> {
        static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
        if let Some(c) = CFG.get() {
            return Ok(tungstenite::Connector::Rustls(c.clone()));
        }
        let p = provider();
        let cfg = ClientConfig::builder_with_provider(p.clone())
            .with_safe_default_protocol_versions()
            .map_err(tls_err)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCert(p)))
            .with_no_client_auth();
        Ok(tungstenite::Connector::Rustls(CFG.get_or_init(|| Arc::new(cfg)).clone()))
    }

    pub(super) fn verified(builder: ureq::AgentBuilder) -> Result<ureq::AgentBuilder> {
        static CFG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
        if let Some(c) = CFG.get() {
            return Ok(builder.tls_config(c.clone()));
        }
        let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let cfg = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(tls_err)?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(builder.tls_config(CFG.get_or_init(|| Arc::new(cfg)).clone()))
    }
}

#[cfg(all(feature = "native-tls", not(feature = "rustls")))]
mod imp {
    use std::sync::Arc;

    use crate::error::{Error, Result};

    pub(super) fn tv_ws_connector() -> Result<tungstenite::Connector> {
        let tls = native_tls::TlsConnector::builder()
            .danger_accept_invalid_certs(true)
            .danger_accept_invalid_hostnames(true)
            .build()
            .map_err(|e| Error::WebSocket(format!("TLS init: {e}")))?;
        Ok(tungstenite::Connector::NativeTls(tls))
    }

    pub(super) fn verified(builder: ureq::AgentBuilder) -> Result<ureq::AgentBuilder> {
        let connector = native_tls::TlsConnector::new()
            .map_err(|e| Error::Http(format!("TLS init failed: {e}")))?;
        Ok(builder.tls_connector(Arc::new(connector)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connectors_build() {
        assert!(tv_ws_connector().is_ok());
        assert!(verified(ureq::AgentBuilder::new()).is_ok());
        assert!(["rustls", "native-tls"].contains(&backend()));
    }
}
