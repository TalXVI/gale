//! Certificate trust for FTPS connections.

use std::sync::{Arc, Mutex};

use eyre::{Context, Result};
use sha2::{Digest, Sha256};
use suppaftp::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};

/// The rustls provider for FTPS client configuration. Prefer the
/// process-wide provider when the application installed one; fall back
/// to aws-lc-rs, which this crate's feature graph always compiles in.
/// Resolving it explicitly keeps this module working in processes that
/// never install a default, where `ClientConfig::builder()` panics on
/// the ambiguous feature set.
pub(super) fn crypto_provider() -> Arc<suppaftp::rustls::crypto::CryptoProvider> {
    suppaftp::rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(suppaftp::rustls::crypto::aws_lc_rs::default_provider()))
}

/// The `ClientConfig` behind an FTPS session. Provider and protocol
/// versions are chosen explicitly for the same reason as
/// [`crypto_provider`]: the plain builder relies on process state this
/// module cannot assume.
pub(super) fn ftps_client_config(verifier: FtpsCertVerifier) -> ClientConfig {
    ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .expect("the aws-lc-rs provider supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth()
}

/// FTPS certificate verification. When a fingerprint pin is configured it
/// is an exact-match requirement: the certificate the server presents
/// must match it whether or not normal CA validation would have
/// succeeded, so a CA-valid *replacement* certificate cannot silently
/// satisfy the pin. Without a pin, normal CA validation decides. When
/// that fails, the observed fingerprint is recorded so the caller can
/// show it and ask the user to trust it explicitly.
///
/// Handshake-signature verification always goes through webpki. Trusting
/// a certificate must never skip checking that the server holds its
/// private key.
#[derive(Debug)]
pub(super) struct FtpsCertVerifier {
    webpki: Arc<suppaftp::rustls::client::WebPkiServerVerifier>,
    pinned: Option<String>,
    observed: Arc<Mutex<Option<String>>>,
}

impl FtpsCertVerifier {
    pub(super) fn new(pinned: Option<String>) -> Result<(Self, Arc<Mutex<Option<String>>>)> {
        let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Self::with_roots(roots, pinned)
    }

    fn with_roots(
        roots: RootCertStore,
        pinned: Option<String>,
    ) -> Result<(Self, Arc<Mutex<Option<String>>>)> {
        let webpki = suppaftp::rustls::client::WebPkiServerVerifier::builder_with_provider(
            Arc::new(roots),
            crypto_provider(),
        )
        .build()
        .context("failed to build certificate verifier")?;
        let observed = Arc::new(Mutex::new(None));

        Ok((
            Self {
                webpki,
                pinned,
                observed: observed.clone(),
            },
            observed,
        ))
    }
}

impl ServerCertVerifier for FtpsCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        let fingerprint = certificate_fingerprint(end_entity);
        *self.observed.lock().unwrap() = Some(fingerprint.clone());

        // The pin is the whole trust decision when configured: exact match
        // or rejection, independent of what CA validation thinks.
        if let Some(pinned) = self.pinned.as_deref() {
            return if pinned == fingerprint {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(TlsError::InvalidCertificate(
                    suppaftp::rustls::CertificateError::Other(suppaftp::rustls::OtherError(
                        Arc::new(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "certificate fingerprint does not match the pinned value",
                        )),
                    )),
                ))
            };
        }

        self.webpki
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

/// Fingerprint of the certificate the server presents: its SHA-256 digest
/// as colon-separated hex, the form `openssl x509 -fingerprint -sha256` and
/// browsers display, so users can check it against their host's records.
/// The same string is stored as the pin.
pub(super) fn certificate_fingerprint(cert: &CertificateDer<'_>) -> String {
    Sha256::digest(cert.as_ref())
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use eyre::Result;
    use suppaftp::rustls::{
        RootCertStore, SignatureScheme,
        client::danger::ServerCertVerifier,
        pki_types::{CertificateDer, ServerName, UnixTime},
    };

    use super::{FtpsCertVerifier, certificate_fingerprint, ftps_client_config};

    /// The connector must initialize in a fresh process without a global provider.
    #[test]
    fn ftps_config_builds_without_a_process_default_provider() {
        const CHILD_ENV: &str = "GALE_FTPS_PROVIDER_CHILD";
        if std::env::var_os(CHILD_ENV).is_some() {
            assert!(suppaftp::rustls::crypto::CryptoProvider::get_default().is_none());
            let (verifier, _) = FtpsCertVerifier::new(None).unwrap();
            let _ = ftps_client_config(verifier);
            return;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "profile::server::remote::ftps_verifier::tests::ftps_config_builds_without_a_process_default_provider"])
            .env(CHILD_ENV, "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }

    // ---------- FTPS certificate verification ----------

    fn self_signed(name: &str) -> rcgen::CertifiedKey {
        rcgen::generate_simple_self_signed(vec![name.to_owned()]).unwrap()
    }

    /// A CA plus a leaf it signed. webpki accepts the leaf when the CA is
    /// in the root store.
    fn ca_signed_leaf(name: &str) -> (RootCertStore, rcgen::CertifiedKey) {
        let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();

        let leaf_params = rcgen::CertificateParams::new(vec![name.to_owned()]).unwrap();
        let leaf_key = rcgen::KeyPair::generate().unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &ca_cert, &ca_key).unwrap();

        let mut roots = RootCertStore::empty();
        roots.add(ca_cert.der().clone()).unwrap();

        (
            roots,
            rcgen::CertifiedKey {
                cert: leaf_cert,
                key_pair: leaf_key,
            },
        )
    }

    fn verifier(
        mut roots: RootCertStore,
        pinned: Option<String>,
    ) -> (FtpsCertVerifier, Arc<std::sync::Mutex<Option<String>>>) {
        // WebPkiServerVerifier requires at least one trust anchor even
        // when a pin bypasses it, so add a throwaway CA to satisfy it.
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        roots
            .add(params.self_signed(&ca_key).unwrap().der().clone())
            .unwrap();
        FtpsCertVerifier::with_roots(roots, pinned).unwrap()
    }

    fn verify(
        verifier: &FtpsCertVerifier,
        cert: &CertificateDer<'static>,
        name: &str,
    ) -> Result<(), suppaftp::rustls::Error> {
        verifier
            .verify_server_cert(
                cert,
                &[],
                &ServerName::try_from(name.to_owned()).unwrap(),
                &[],
                UnixTime::now(),
            )
            .map(|_| ())
    }

    #[test]
    fn ca_valid_replacement_cannot_satisfy_a_pin() {
        // The certificate chains to a trusted CA, but it is not the pinned
        // one. A pin is exact-match, not "any CA-valid cert".
        let (roots, certified) = ca_signed_leaf("ftps.example.com");
        let other = self_signed("other");
        let (verifier, _) = verifier(roots, Some(certificate_fingerprint(other.cert.der())));

        assert!(verify(&verifier, certified.cert.der(), "ftps.example.com").is_err());
    }

    #[test]
    fn ca_valid_cert_is_accepted_without_a_pin() {
        let (roots, certified) = ca_signed_leaf("ftps.example.com");
        let (verifier, _) = verifier(roots, None);

        verify(&verifier, certified.cert.der(), "ftps.example.com").unwrap();
    }

    #[test]
    fn unpinned_self_signed_cert_is_rejected_and_observed() {
        let certified = self_signed("ftps.local");
        let (verifier, observed) = verifier(RootCertStore::empty(), None);

        assert!(verify(&verifier, certified.cert.der(), "ftps.local").is_err());
        // The fingerprint is surfaced so the user can make an explicit
        // trust decision.
        assert_eq!(
            observed.lock().unwrap().as_deref(),
            Some(certificate_fingerprint(certified.cert.der()).as_str())
        );
    }

    #[test]
    fn mismatched_pin_rejects_even_when_self_signed() {
        let certified = self_signed("ftps.local");
        let other = self_signed("other.local");
        let (verifier, _) = verifier(
            RootCertStore::empty(),
            Some(certificate_fingerprint(other.cert.der())),
        );

        assert!(verify(&verifier, certified.cert.der(), "ftps.local").is_err());
    }

    // ---------- real TLS handshakes through the verifier ----------

    use suppaftp::rustls::{
        ClientConfig, ClientConnection, ServerConfig, ServerConnection,
        pki_types::PrivatePkcs8KeyDer,
        server::ResolvesServerCert,
        sign::{CertifiedKey, Signer, SigningKey},
    };

    fn provider() -> Arc<suppaftp::rustls::crypto::CryptoProvider> {
        Arc::new(suppaftp::rustls::crypto::aws_lc_rs::default_provider())
    }

    fn client_config(verifier: FtpsCertVerifier) -> Arc<ClientConfig> {
        Arc::new(
            ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth(),
        )
    }

    fn client_config_tls12(verifier: FtpsCertVerifier) -> Arc<ClientConfig> {
        Arc::new(
            ClientConfig::builder_with_provider(provider())
                .with_protocol_versions(&[&suppaftp::rustls::version::TLS12])
                .unwrap()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(verifier))
                .with_no_client_auth(),
        )
    }

    fn server_config(certified: &rcgen::CertifiedKey) -> Arc<ServerConfig> {
        let key = PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der());
        Arc::new(
            ServerConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![certified.cert.der().clone()], key.into())
                .unwrap(),
        )
    }

    /// A signing key that produces well-formed but wrong ECDSA signatures.
    /// The server presents a valid certificate but cannot prove possession
    /// of its private key.
    #[derive(Debug)]
    struct ForgedKey;

    impl SigningKey for ForgedKey {
        fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
            offered
                .contains(&SignatureScheme::ECDSA_NISTP256_SHA256)
                .then(|| Box::new(ForgedKey) as Box<dyn Signer>)
        }

        fn algorithm(&self) -> suppaftp::rustls::SignatureAlgorithm {
            suppaftp::rustls::SignatureAlgorithm::ECDSA
        }
    }

    impl Signer for ForgedKey {
        fn sign(&self, _message: &[u8]) -> Result<Vec<u8>, suppaftp::rustls::Error> {
            // DER-shaped ECDSA signature (SEQUENCE of two INTEGERs) whose
            // scalars cannot be the real transcript signature.
            let mut signature = vec![0x30, 0x44, 0x02, 0x20];
            signature.extend_from_slice(&[7u8; 32]);
            signature.extend_from_slice(&[0x02, 0x20]);
            signature.extend_from_slice(&[9u8; 32]);
            Ok(signature)
        }

        fn scheme(&self) -> SignatureScheme {
            SignatureScheme::ECDSA_NISTP256_SHA256
        }
    }

    #[derive(Debug)]
    struct FixedResolver(Arc<CertifiedKey>);

    impl ResolvesServerCert for FixedResolver {
        fn resolve(
            &self,
            _client_hello: suppaftp::rustls::server::ClientHello<'_>,
        ) -> Option<Arc<CertifiedKey>> {
            Some(self.0.clone())
        }
    }

    fn forged_server_config(certified: &rcgen::CertifiedKey) -> Arc<ServerConfig> {
        let key = Arc::new(CertifiedKey::new(
            vec![certified.cert.der().clone()],
            Arc::new(ForgedKey),
        ));
        Arc::new(
            ServerConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_cert_resolver(Arc::new(FixedResolver(key))),
        )
    }

    /// Pumps TLS records between an in-memory client and server until the
    /// handshake completes or a side errors.
    fn pump(
        client: &mut ClientConnection,
        server: &mut ServerConnection,
    ) -> Result<(), suppaftp::rustls::Error> {
        loop {
            while client.wants_write() {
                let mut buf = Vec::new();
                client.write_tls(&mut buf).unwrap();
                server.read_tls(&mut &buf[..]).unwrap();
            }
            server.process_new_packets()?;
            while server.wants_write() {
                let mut buf = Vec::new();
                server.write_tls(&mut buf).unwrap();
                client.read_tls(&mut &buf[..]).unwrap();
            }
            client.process_new_packets()?;
            if !client.is_handshaking() && !server.is_handshaking() {
                return Ok(());
            }
            assert!(
                client.wants_write() || server.wants_write(),
                "handshake stalled"
            );
        }
    }

    fn handshake(
        client_config: Arc<ClientConfig>,
        server_config: Arc<ServerConfig>,
        name: &str,
    ) -> Result<(), suppaftp::rustls::Error> {
        let mut client = ClientConnection::new(
            client_config,
            ServerName::try_from(name.to_owned()).unwrap(),
        )
        .unwrap();
        let mut server = ServerConnection::new(server_config).unwrap();
        pump(&mut client, &mut server)
    }

    #[test]
    fn pinned_cert_completes_a_real_tls_handshake() {
        // A pinned self-signed certificate, with a server holding its
        // private key, completes the handshake under both TLS versions.
        for config in [client_config, client_config_tls12] {
            let certified = self_signed("ftps.local");
            let pin = certificate_fingerprint(certified.cert.der());
            let (verifier, _) = verifier(RootCertStore::empty(), Some(pin));

            handshake(config(verifier), server_config(&certified), "ftps.local")
                .expect("pinned handshake failed");
        }
    }

    #[test]
    fn forged_handshake_signature_fails_even_when_cert_is_pinned() {
        // A pin must not turn into `HandshakeSignatureValid::assertion()`.
        // The certificate is pinned, but the server signs with garbage.
        // The handshake must fail, proving verify_tls1x_signature still does real crypto.
        for config in [client_config, client_config_tls12] {
            let certified = self_signed("ftps.local");
            let pin = certificate_fingerprint(certified.cert.der());
            let (verifier, _) = verifier(RootCertStore::empty(), Some(pin));

            assert!(
                handshake(
                    config(verifier),
                    forged_server_config(&certified),
                    "ftps.local"
                )
                .is_err(),
                "forged handshake signature was accepted"
            );
        }
    }

    #[test]
    fn unpinned_cert_fails_the_handshake() {
        let certified = self_signed("ftps.local");
        let (verifier, _) = verifier(RootCertStore::empty(), None);

        assert!(
            handshake(
                client_config(verifier),
                server_config(&certified),
                "ftps.local"
            )
            .is_err()
        );
    }
}
