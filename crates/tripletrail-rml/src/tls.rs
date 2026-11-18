//! TLS for PostgreSQL connections, with libpq's `sslmode`s (rustls, ring).
//!
//! - `disable`: no TLS.
//! - `prefer` (the default) and `require`: encrypted without checking the server's
//!   certificate, as libpq does. `prefer` falls back to no TLS when the server has none.
//! - `verify-ca`: the certificate must chain to a trusted root.
//! - `verify-full`: the certificate must also name the host.
//!
//! The trusted roots are the Mozilla roots (webpki-roots), or the PEM file of `sslrootcert`.

use crate::RmlError;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore};
use std::sync::Arc;
use tokio_postgres_rustls::MakeRustlsConnect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

/// A connection string without the TLS settings tokio-postgres does not read itself
/// (`verify-ca`, `verify-full`, `sslrootcert`), with the settings taken out.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Split {
    pub connection: String,
    pub mode: Mode,
    pub root_cert: Option<String>,
}

/// Takes `sslmode` and `sslrootcert` out of a URL (`?sslmode=require&…`) or `key=value`
/// connection string; `sslmode` goes back in as one tokio-postgres reads (`verify-*` as
/// `require`).
pub(crate) fn split(connection: &str) -> Result<Split, RmlError> {
    let mut mode = Mode::Prefer;
    let mut root_cert = None;
    let mut take = |key: &str, value: &str| -> Result<bool, RmlError> {
        match key {
            "sslmode" => {
                mode = match value {
                    "disable" => Mode::Disable,
                    "allow" | "prefer" => Mode::Prefer,
                    "require" => Mode::Require,
                    "verify-ca" => Mode::VerifyCa,
                    "verify-full" => Mode::VerifyFull,
                    v => {
                        return Err(RmlError::Mapping(format!("unknown sslmode {v}")));
                    }
                };
                Ok(true)
            }
            "sslrootcert" => {
                root_cert = Some(value.to_owned());
                Ok(true)
            }
            _ => Ok(false),
        }
    };
    let mut kept = Vec::new();
    let base = if connection.contains("://") {
        let (base, query) = connection.split_once('?').unwrap_or((connection, ""));
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if !take(k, v)? {
                kept.push(pair.to_owned());
            }
        }
        base.to_owned()
    } else {
        for pair in connection.split_whitespace() {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if !take(k.trim(), v.trim_matches('\''))? {
                kept.push(pair.to_owned());
            }
        }
        String::new()
    };
    let tokio_mode = match mode {
        Mode::Disable => "disable",
        Mode::Prefer => "prefer",
        _ => "require",
    };
    kept.push(format!("sslmode={tokio_mode}"));
    let connection = if connection.contains("://") {
        format!("{base}?{}", kept.join("&"))
    } else {
        kept.join(" ")
    };
    Ok(Split {
        connection,
        mode,
        root_cert,
    })
}

/// The TLS connector for `mode`.
pub(crate) fn connector(
    mode: Mode,
    root_cert: Option<&str>,
) -> Result<MakeRustlsConnect, RmlError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| RmlError::Mapping(format!("TLS: {e}")))?;
    let verifier: Arc<dyn ServerCertVerifier> = match mode {
        Mode::Disable | Mode::Prefer | Mode::Require => Arc::new(Unverified(provider)),
        Mode::VerifyCa | Mode::VerifyFull => {
            let mut roots = RootCertStore::empty();
            match root_cert {
                None => roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned()),
                Some(path) => {
                    let error = |e: String| RmlError::Source {
                        location: path.to_owned(),
                        message: format!("sslrootcert: {e}"),
                    };
                    for cert in
                        CertificateDer::pem_file_iter(path).map_err(|e| error(e.to_string()))?
                    {
                        roots
                            .add(cert.map_err(|e| error(e.to_string()))?)
                            .map_err(|e| error(e.to_string()))?;
                    }
                }
            }
            let webpki = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
                .build()
                .map_err(|e| RmlError::Mapping(format!("TLS: {e}")))?;
            if mode == Mode::VerifyCa {
                Arc::new(AnyName(webpki))
            } else {
                webpki
            }
        }
    };
    let config = builder
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

/// `prefer` and `require`: any certificate (the signatures of the handshake are still
/// checked).
#[derive(Debug)]
struct Unverified(Arc<CryptoProvider>);

impl ServerCertVerifier for Unverified {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// `verify-ca`: a trusted chain, for any host name.
#[derive(Debug)]
struct AnyName(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for AnyName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
        {
            Err(Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            r => r,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssl_settings_are_taken_out_of_connection_strings() {
        let s =
            split("postgresql://u@h/db?sslmode=verify-full&sslrootcert=/ca.pem&application_name=x")
                .unwrap();
        assert_eq!(s.mode, Mode::VerifyFull);
        assert_eq!(s.root_cert.as_deref(), Some("/ca.pem"));
        assert_eq!(
            s.connection,
            "postgresql://u@h/db?application_name=x&sslmode=require"
        );
        let s = split("host=h dbname=db sslmode=disable").unwrap();
        assert_eq!(
            (s.mode, s.connection.as_str()),
            (Mode::Disable, "host=h dbname=db sslmode=disable")
        );
        let s = split("postgresql://h/db").unwrap();
        assert_eq!(
            (s.mode, s.connection.as_str()),
            (Mode::Prefer, "postgresql://h/db?sslmode=prefer")
        );
        assert!(split("postgresql://h/db?sslmode=sometimes").is_err());
        for mode in [Mode::Prefer, Mode::VerifyCa, Mode::VerifyFull] {
            assert!(connector(mode, None).is_ok());
        }
    }
}
