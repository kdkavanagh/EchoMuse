//! The device-link TLS credentials (spec §3.1.2).
//!
//! The TLS directory holds four PEM files: the controller's certificate
//! authority (`ca.pem`, `ca.key`) and the certificate the TLS listener presents
//! (`server.pem`, `server.key`). Devices trust only `ca.pem`, which the
//! controller pushes to them, and check that the server certificate names
//! [`TLS_SERVER_NAME`] (`device/internal/client/tlscreds.go`).
//!
//! Devices pin the CA, so an existing `ca.pem` is never replaced and an
//! existing `ca.key` is never overwritten. The server certificate pins nothing,
//! so a missing or unusable one is issued again from the CA. Everything is
//! created on first use: EC P-256 keys in PKCS #8, ECDSA with SHA-256.
//!
//! Validity is deliberately unbounded. An Echo can boot with a wrong clock and
//! the firmware only raises its verification clock to the firmware's build time,
//! so certificates are valid from 2000-01-01 and carry RFC 5280's "no
//! well-defined expiration" date, 9999-12-31 23:59:59 UTC.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa,
    Issuer, KeyPair, KeyUsagePurpose, PKCS_ECDSA_P256_SHA256,
};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{RootCertStore, ServerConfig};
use thiserror::Error;

/// The DNS name in the server certificate. Devices verify against this name
/// whatever address discovery gave them; it is an identity label, not a
/// resolvable host.
pub const TLS_SERVER_NAME: &str = "echomuse-controller";

/// The common name of a CA this controller creates.
pub const CA_COMMON_NAME: &str = "EchoMuse Controller CA";

/// One of the four credential files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlsFile {
    /// `ca.pem`: the CA certificate devices pin.
    CaCert,
    /// `ca.key`: the CA's private key.
    CaKey,
    /// `server.pem`: the listener's certificate chain.
    ServerCert,
    /// `server.key`: the listener's private key.
    ServerKey,
}

impl TlsFile {
    /// The file name inside the TLS directory.
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::CaCert => "ca.pem",
            Self::CaKey => "ca.key",
            Self::ServerCert => "server.pem",
            Self::ServerKey => "server.key",
        }
    }

    /// Private keys are readable by the owner only; certificates by anyone.
    const fn mode(self) -> u32 {
        match self {
            Self::CaKey | Self::ServerKey => 0o600,
            Self::CaCert | Self::ServerCert => 0o644,
        }
    }

    fn path(self, dir: &Path) -> PathBuf {
        dir.join(self.file_name())
    }
}

/// Why the existing server certificate could not be used.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerCertProblem {
    /// `server.pem` or `server.key` holds no usable PEM.
    Unreadable(String),
    /// The key does not belong to the certificate, or rustls refused the pair.
    Unusable(String),
    /// The certificate does not verify against the CA for [`TLS_SERVER_NAME`].
    NotTrusted(String),
}

/// Why a server certificate was issued on this start.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IssueReason {
    /// The CA was created on this start.
    NewCa,
    /// `server.pem` or `server.key` was missing.
    Missing,
    /// The existing pair could not be used.
    Replaced(ServerCertProblem),
}

/// What happened to the server certificate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ServerCertStatus {
    /// The existing pair was loaded unchanged.
    Loaded,
    /// A new pair was issued from the CA and written.
    Issued(IssueReason),
}

/// Why the credentials could not be loaded or created.
#[derive(Debug, Error)]
pub enum TlsError {
    /// A file or directory operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path involved.
        path: PathBuf,
        /// The failure.
        source: io::Error,
    },
    /// `ca.pem` is missing while other credential files exist. Creating a new
    /// CA would orphan devices that trust the old one, so nothing is changed.
    #[error(
        "ca.pem is missing but {present:?} exist; restore ca.pem, or delete the directory to rotate the CA"
    )]
    CaCertMissing {
        /// The credential files that are present.
        present: Vec<TlsFile>,
    },
    /// `ca.pem` does not hold exactly one usable certificate.
    #[error("ca.pem is not a usable CA certificate: {0}")]
    CaCertUnusable(String),
    /// A server certificate must be issued but `ca.key` is missing.
    #[error("cannot issue a server certificate ({reason:?}) because ca.key is missing")]
    CaKeyMissing {
        /// Why a certificate had to be issued.
        reason: IssueReason,
    },
    /// `ca.key` is not a PKCS #8 EC P-256 key.
    #[error("ca.key is not a usable PKCS #8 EC P-256 key: {0}")]
    CaKeyUnusable(String),
    /// `ca.key` does not belong to `ca.pem`.
    #[error("ca.key does not belong to the certificate in ca.pem")]
    CaKeyMismatch,
    /// Key or certificate generation failed.
    #[error("certificate generation failed: {0}")]
    Generate(#[from] rcgen::Error),
    /// A freshly issued certificate was refused; this indicates a bug or an
    /// unusual CA certificate.
    #[error("the issued server certificate was refused: {0}")]
    IssuedCertRefused(String),
}

/// The loaded credentials for the TLS device listener.
#[derive(Debug)]
pub struct DeviceLinkTls {
    dir: PathBuf,
    ca_pem: String,
    server_config: Arc<ServerConfig>,
    created_ca: bool,
    server_cert: ServerCertStatus,
}

impl DeviceLinkTls {
    /// Loads the credentials in `dir`, creating what is missing (see the module
    /// documentation for the rules).
    pub fn load_or_create(dir: &Path) -> Result<Self, TlsError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let ca_cert_path = TlsFile::CaCert.path(dir);

        let created_ca = !exists(&ca_cert_path)?;
        if created_ca {
            let mut present = Vec::new();
            for file in [TlsFile::CaKey, TlsFile::ServerCert, TlsFile::ServerKey] {
                if exists(&file.path(dir))? {
                    present.push(file);
                }
            }
            if !present.is_empty() {
                return Err(TlsError::CaCertMissing { present });
            }
            create_ca(dir)?;
        }

        let ca_pem =
            fs::read_to_string(&ca_cert_path).map_err(|source| io_error(&ca_cert_path, source))?;
        let ca_der = single_certificate(&ca_pem)?;
        let verifier = verifier_for(&ca_der, &provider)?;

        let server_cert_path = TlsFile::ServerCert.path(dir);
        let server_key_path = TlsFile::ServerKey.path(dir);
        let reason = if created_ca {
            IssueReason::NewCa
        } else if exists(&server_cert_path)? && exists(&server_key_path)? {
            let cert_pem = fs::read(&server_cert_path)
                .map_err(|source| io_error(&server_cert_path, source))?;
            let key_pem =
                fs::read(&server_key_path).map_err(|source| io_error(&server_key_path, source))?;
            match load_server(&cert_pem, &key_pem, &provider, verifier.as_ref()) {
                Ok(server_config) => {
                    return Ok(Self {
                        dir: dir.to_owned(),
                        ca_pem,
                        server_config,
                        created_ca,
                        server_cert: ServerCertStatus::Loaded,
                    });
                }
                Err(problem) => IssueReason::Replaced(problem),
            }
        } else {
            IssueReason::Missing
        };

        let server_config =
            issue_server(dir, &ca_der, &provider, verifier.as_ref(), reason.clone())?;
        Ok(Self {
            dir: dir.to_owned(),
            ca_pem,
            server_config,
            created_ca,
            server_cert: ServerCertStatus::Issued(reason),
        })
    }

    /// The TLS directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The CA certificate as PEM, exactly as stored: what devices are given to pin.
    pub fn ca_pem(&self) -> &str {
        &self.ca_pem
    }

    /// The rustls configuration for the TLS device listener: TLS 1.2 or 1.3,
    /// no client certificates, presenting the server certificate chain.
    pub fn server_config(&self) -> Arc<ServerConfig> {
        Arc::clone(&self.server_config)
    }

    /// Whether this start created the CA.
    pub fn created_ca(&self) -> bool {
        self.created_ca
    }

    /// What happened to the server certificate on this start.
    pub fn server_cert(&self) -> &ServerCertStatus {
        &self.server_cert
    }
}

fn io_error(path: &Path, source: io::Error) -> TlsError {
    TlsError::Io {
        path: path.to_owned(),
        source,
    }
}

fn exists(path: &Path) -> Result<bool, TlsError> {
    path.try_exists().map_err(|source| io_error(path, source))
}

/// Writes through a temporary file and a rename, so a crash leaves the old file
/// or none, never a truncated one.
fn write_atomic(dir: &Path, file: TlsFile, contents: &[u8]) -> Result<(), TlsError> {
    let path = file.path(dir);
    let temporary = dir.join(format!("{}.tmp", file.file_name()));
    let mut out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(file.mode())
        .open(&temporary)
        .map_err(|source| io_error(&temporary, source))?;
    out.write_all(contents)
        .and_then(|()| out.sync_all())
        .map_err(|source| io_error(&temporary, source))?;
    drop(out);
    fs::rename(&temporary, &path).map_err(|source| io_error(&path, source))?;
    File::open(dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(dir, source))
}

fn validity(params: &mut CertificateParams) {
    params.not_before = rcgen::date_time_ymd(2000, 1, 1);
    params.not_after = rcgen::date_time_ymd(9999, 12, 31) + time::Duration::seconds(86_399);
}

/// Creates `ca.key` and then `ca.pem`. A certificate is written only after its
/// key, so `ca.pem` never exists without the key that issued it.
fn create_ca(dir: &Path) -> Result<(), TlsError> {
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|source| io_error(dir, source))?;
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, CA_COMMON_NAME);
    params.distinguished_name = name;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    validity(&mut params);
    let cert = params.self_signed(&key)?;
    write_atomic(dir, TlsFile::CaKey, key.serialize_pem().as_bytes())?;
    write_atomic(dir, TlsFile::CaCert, cert.pem().as_bytes())
}

fn single_certificate(pem: &str) -> Result<CertificateDer<'static>, TlsError> {
    let mut certificates = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| TlsError::CaCertUnusable(error.to_string()))?;
    match certificates.len() {
        1 => Ok(certificates.remove(0)),
        count => Err(TlsError::CaCertUnusable(format!(
            "expected one certificate, found {count}"
        ))),
    }
}

fn verifier_for(
    ca: &CertificateDer<'static>,
    provider: &Arc<CryptoProvider>,
) -> Result<Arc<WebPkiServerVerifier>, TlsError> {
    let mut roots = RootCertStore::empty();
    roots
        .add(ca.clone())
        .map_err(|error| TlsError::CaCertUnusable(error.to_string()))?;
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(provider))
        .build()
        .map_err(|error| TlsError::CaCertUnusable(error.to_string()))
}

fn listener_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    provider: &Arc<CryptoProvider>,
) -> Result<Arc<ServerConfig>, rustls::Error> {
    let config = ServerConfig::builder_with_provider(Arc::clone(provider))
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])?
        .with_no_client_auth()
        .with_single_cert(chain, key)?;
    Ok(Arc::new(config))
}

/// Checks that `chain` verifies against the CA for [`TLS_SERVER_NAME`], as a
/// device does.
fn verify_chain(
    verifier: &dyn ServerCertVerifier,
    chain: &[CertificateDer<'static>],
) -> Result<(), String> {
    let (leaf, intermediates) = chain
        .split_first()
        .ok_or("server.pem holds no certificate")?;
    let name = ServerName::try_from(TLS_SERVER_NAME).map_err(|error| error.to_string())?;
    verifier
        .verify_server_cert(leaf, intermediates, &name, &[], UnixTime::now())
        .map(drop)
        .map_err(|error| error.to_string())
}

fn load_server(
    cert_pem: &[u8],
    key_pem: &[u8],
    provider: &Arc<CryptoProvider>,
    verifier: &dyn ServerCertVerifier,
) -> Result<Arc<ServerConfig>, ServerCertProblem> {
    let chain = CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| ServerCertProblem::Unreadable(format!("server.pem: {error}")))?;
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|error| ServerCertProblem::Unreadable(format!("server.key: {error}")))?;
    verify_chain(verifier, &chain).map_err(ServerCertProblem::NotTrusted)?;
    listener_config(chain, key, provider)
        .map_err(|error| ServerCertProblem::Unusable(error.to_string()))
}

/// Loads `ca.key` and checks that it is the P-256 key of `ca`.
fn load_ca_key(
    dir: &Path,
    ca: &CertificateDer<'static>,
    reason: IssueReason,
) -> Result<KeyPair, TlsError> {
    let path = TlsFile::CaKey.path(dir);
    if !exists(&path)? {
        return Err(TlsError::CaKeyMissing { reason });
    }
    let pem = fs::read_to_string(&path).map_err(|source| io_error(&path, source))?;
    let key =
        KeyPair::from_pem(&pem).map_err(|error| TlsError::CaKeyUnusable(error.to_string()))?;
    if key.algorithm() != &PKCS_ECDSA_P256_SHA256 {
        return Err(TlsError::CaKeyUnusable(format!("{:?}", key.algorithm())));
    }
    let (_, parsed) = x509_parser::parse_x509_certificate(ca)
        .map_err(|error| TlsError::CaCertUnusable(error.to_string()))?;
    if parsed.public_key().subject_public_key.data.as_ref() != key.public_key_raw() {
        return Err(TlsError::CaKeyMismatch);
    }
    Ok(key)
}

/// Issues a new server certificate from the CA, checks it as a device would,
/// and writes `server.key` and then `server.pem`. A crash between the two
/// leaves a pair that does not match, which the next start replaces.
fn issue_server(
    dir: &Path,
    ca: &CertificateDer<'static>,
    provider: &Arc<CryptoProvider>,
    verifier: &dyn ServerCertVerifier,
    reason: IssueReason,
) -> Result<Arc<ServerConfig>, TlsError> {
    let ca_key = load_ca_key(dir, ca, reason)?;
    // The issuer name is taken from ca.pem itself, so the new certificate's
    // issuer matches the CA's subject exactly, whatever string types it uses.
    let issuer = Issuer::from_ca_cert_der(ca, &ca_key)?;

    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let mut params = CertificateParams::new(vec![TLS_SERVER_NAME.to_owned()])?;
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, TLS_SERVER_NAME);
    params.distinguished_name = name;
    params.is_ca = IsCa::ExplicitNoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = true;
    validity(&mut params);
    let cert = params.signed_by(&key, &issuer)?;

    let chain = vec![cert.der().clone()];
    verify_chain(verifier, &chain).map_err(TlsError::IssuedCertRefused)?;
    let private = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
    let config = listener_config(chain, private, provider)
        .map_err(|error| TlsError::IssuedCertRefused(error.to_string()))?;

    write_atomic(dir, TlsFile::ServerKey, key.serialize_pem().as_bytes())?;
    write_atomic(dir, TlsFile::ServerCert, cert.pem().as_bytes())?;
    Ok(config)
}
