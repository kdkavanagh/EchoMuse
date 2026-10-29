//! Device-link TLS credentials: creation, reload, CA preservation, repair of the
//! server certificate, and handshakes as a device performs them.
//!
//! The fixtures under `tests/fixtures/tls` were made with OpenSSL 3.5 from
//! `ca.cnf` and `printable.cnf` in that directory:
//! - `standard`: P-256 PKCS #8 keys, UTF8String names, a CA with
//!   `basicConstraints=CA:TRUE,pathlen:0` and `keyCertSign,cRLSign`, and a leaf
//!   for `DNS:echomuse-controller` with `serverAuth`, valid 2016-08-07 to
//!   9999-12-31 (`openssl genpkey -algorithm EC -pkeyopt
//!   ec_paramgen_curve:P-256`, `openssl req -x509 -extensions ca_ext`,
//!   `openssl x509 -req -extensions leaf_ext`);
//! - `sec1_server_key`: the same files with `server.key` converted to SEC1
//!   (`openssl ec`);
//! - `printable_ca`: only `ca.pem` and `ca.key`, with the subject
//!   `C=GB, O=Example Household, CN=Printable Test CA` in PrintableString.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use echomuse_config::TLS_SERVER_NAME;
use echomuse_config::tls::{
    DeviceLinkTls, IssueReason, ServerCertProblem, ServerCertStatus, TlsError, TlsFile,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, RootCertStore, ServerConfig, ServerConnection};
use x509_parser::prelude::*;

const ALL_FILES: [TlsFile; 4] = [
    TlsFile::CaCert,
    TlsFile::CaKey,
    TlsFile::ServerCert,
    TlsFile::ServerKey,
];

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

/// Copies a fixture set into a fresh directory so loading cannot touch the repository.
fn copy_fixture(name: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for entry in fs::read_dir(fixture(name)).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
    }
    dir
}

fn read(dir: &Path, file: TlsFile) -> Vec<u8> {
    fs::read(dir.join(file.file_name())).unwrap()
}

fn snapshot(dir: &Path) -> Vec<Option<Vec<u8>>> {
    ALL_FILES
        .iter()
        .map(|file| fs::read(dir.join(file.file_name())).ok())
        .collect()
}

fn der(pem: &[u8]) -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(pem).unwrap()
}

/// A device's client configuration: only the given CA is trusted.
fn device_client(
    ca_pem: &[u8],
    version: &'static rustls::SupportedProtocolVersion,
) -> ClientConfig {
    let mut roots = RootCertStore::empty();
    roots.add(der(ca_pem)).unwrap();
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[version])
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth()
}

/// Runs a TLS handshake in memory between a device client and the listener.
fn handshake(
    server: Arc<ServerConfig>,
    client: ClientConfig,
) -> Result<rustls::ProtocolVersion, rustls::Error> {
    let name = ServerName::try_from(TLS_SERVER_NAME).unwrap();
    let mut client = ClientConnection::new(Arc::new(client), name)?;
    let mut server = ServerConnection::new(server)?;
    for _ in 0..16 {
        let mut wire = Vec::new();
        client.write_tls(&mut wire).unwrap();
        server.read_tls(&mut wire.as_slice()).unwrap();
        server.process_new_packets()?;
        let mut wire = Vec::new();
        server.write_tls(&mut wire).unwrap();
        client.read_tls(&mut wire.as_slice()).unwrap();
        client.process_new_packets()?;
        if !client.is_handshaking() && !server.is_handshaking() {
            return Ok(client.protocol_version().unwrap());
        }
    }
    panic!("handshake did not finish");
}

#[test]
fn first_start_creates_a_ca_and_a_server_certificate() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("data/tls");
    let tls = DeviceLinkTls::load_or_create(&dir).unwrap();
    assert!(tls.created_ca());
    assert_eq!(
        tls.server_cert(),
        &ServerCertStatus::Issued(IssueReason::NewCa)
    );
    assert_eq!(tls.ca_pem().as_bytes(), read(&dir, TlsFile::CaCert));

    let mode = |file: TlsFile| {
        fs::metadata(dir.join(file.file_name()))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(TlsFile::CaKey), 0o600);
    assert_eq!(mode(TlsFile::ServerKey), 0o600);
    assert_eq!(mode(TlsFile::CaCert), 0o644);
    assert_eq!(mode(TlsFile::ServerCert), 0o644);
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert!(
        fs::read_dir(&dir).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".tmp"))
    );

    for key in [TlsFile::CaKey, TlsFile::ServerKey] {
        assert!(read(&dir, key).starts_with(b"-----BEGIN PRIVATE KEY-----"));
    }
}

#[test]
fn created_certificates_carry_the_device_link_profile() {
    let dir = tempfile::tempdir().unwrap();
    DeviceLinkTls::load_or_create(dir.path()).unwrap();
    let ca_der = der(&read(dir.path(), TlsFile::CaCert));
    let leaf_der = der(&read(dir.path(), TlsFile::ServerCert));
    let (_, ca) = parse_x509_certificate(&ca_der).unwrap();
    let (_, leaf) = parse_x509_certificate(&leaf_der).unwrap();

    for cert in [&ca, &leaf] {
        assert_eq!(
            cert.signature_algorithm.algorithm,
            x509_parser::oid_registry::OID_SIG_ECDSA_WITH_SHA256
        );
        let curve = cert
            .public_key()
            .algorithm
            .parameters
            .as_ref()
            .unwrap()
            .as_oid()
            .unwrap();
        assert_eq!(curve, x509_parser::oid_registry::OID_EC_P256);
        assert_eq!(cert.validity().not_before.to_datetime().year(), 2000);
        assert_eq!(cert.validity().not_after.to_datetime().year(), 9999);
    }

    let constraints = ca.basic_constraints().unwrap().unwrap();
    assert!(constraints.critical && constraints.value.ca);
    assert_eq!(constraints.value.path_len_constraint, Some(0));
    let usage = ca.key_usage().unwrap().unwrap();
    assert!(usage.value.key_cert_sign() && usage.value.crl_sign());
    assert_eq!(ca.subject().to_string(), "CN=EchoMuse Controller CA");

    assert_eq!(leaf.issuer().as_raw(), ca.subject().as_raw());
    assert!(!leaf.basic_constraints().unwrap().unwrap().value.ca);
    let names = &leaf
        .subject_alternative_name()
        .unwrap()
        .unwrap()
        .value
        .general_names;
    assert_eq!(names, &[GeneralName::DNSName(TLS_SERVER_NAME)]);
    assert!(
        leaf.extended_key_usage()
            .unwrap()
            .unwrap()
            .value
            .server_auth
    );
}

#[test]
fn second_start_loads_everything_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    DeviceLinkTls::load_or_create(dir.path()).unwrap();
    let before = snapshot(dir.path());
    let tls = DeviceLinkTls::load_or_create(dir.path()).unwrap();
    assert!(!tls.created_ca());
    assert_eq!(tls.server_cert(), &ServerCertStatus::Loaded);
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn a_device_completes_a_handshake_over_tls_1_2_and_1_3() {
    let dir = tempfile::tempdir().unwrap();
    let tls = DeviceLinkTls::load_or_create(dir.path()).unwrap();
    let ca = tls.ca_pem().as_bytes();
    assert_eq!(
        handshake(
            tls.server_config(),
            device_client(ca, &rustls::version::TLS12)
        )
        .unwrap(),
        rustls::ProtocolVersion::TLSv1_2
    );
    assert_eq!(
        handshake(
            tls.server_config(),
            device_client(ca, &rustls::version::TLS13)
        )
        .unwrap(),
        rustls::ProtocolVersion::TLSv1_3
    );
}

#[test]
fn a_device_pinning_another_ca_refuses_the_listener() {
    let ours = tempfile::tempdir().unwrap();
    let theirs = tempfile::tempdir().unwrap();
    let tls = DeviceLinkTls::load_or_create(ours.path()).unwrap();
    let other = DeviceLinkTls::load_or_create(theirs.path()).unwrap();
    let error = handshake(
        tls.server_config(),
        device_client(other.ca_pem().as_bytes(), &rustls::version::TLS13),
    )
    .unwrap_err();
    assert!(
        matches!(error, rustls::Error::InvalidCertificate(_)),
        "{error:?}"
    );
}

#[test]
fn a_missing_server_certificate_is_reissued_from_the_existing_ca() {
    let dir = tempfile::tempdir().unwrap();
    DeviceLinkTls::load_or_create(dir.path()).unwrap();
    let ca_pem = read(dir.path(), TlsFile::CaCert);
    let ca_key = read(dir.path(), TlsFile::CaKey);
    let old_leaf = read(dir.path(), TlsFile::ServerCert);
    fs::remove_file(dir.path().join("server.pem")).unwrap();

    let tls = DeviceLinkTls::load_or_create(dir.path()).unwrap();
    assert!(!tls.created_ca());
    assert_eq!(
        tls.server_cert(),
        &ServerCertStatus::Issued(IssueReason::Missing)
    );
    assert_eq!(read(dir.path(), TlsFile::CaCert), ca_pem);
    assert_eq!(read(dir.path(), TlsFile::CaKey), ca_key);
    assert_ne!(read(dir.path(), TlsFile::ServerCert), old_leaf);
    handshake(
        tls.server_config(),
        device_client(&ca_pem, &rustls::version::TLS13),
    )
    .unwrap();
}

#[test]
fn a_mismatched_server_pair_is_replaced() {
    // A crash between writing server.key and server.pem leaves this state.
    let dir = tempfile::tempdir().unwrap();
    DeviceLinkTls::load_or_create(dir.path()).unwrap();
    let ca_pem = read(dir.path(), TlsFile::CaCert);
    let stray = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
    fs::write(dir.path().join("server.key"), stray.serialize_pem()).unwrap();

    let tls = DeviceLinkTls::load_or_create(dir.path()).unwrap();
    assert!(matches!(
        tls.server_cert(),
        ServerCertStatus::Issued(IssueReason::Replaced(ServerCertProblem::Unusable(_)))
    ));
    assert_eq!(read(dir.path(), TlsFile::CaCert), ca_pem);
    handshake(
        tls.server_config(),
        device_client(&ca_pem, &rustls::version::TLS13),
    )
    .unwrap();
}

#[test]
fn a_server_certificate_from_another_ca_is_replaced() {
    let dir = copy_fixture("standard");
    let other = tempfile::tempdir().unwrap();
    DeviceLinkTls::load_or_create(other.path()).unwrap();
    for file in [TlsFile::ServerCert, TlsFile::ServerKey] {
        fs::copy(
            other.path().join(file.file_name()),
            dir.path().join(file.file_name()),
        )
        .unwrap();
    }
    let tls = DeviceLinkTls::load_or_create(dir.path()).unwrap();
    assert!(matches!(
        tls.server_cert(),
        ServerCertStatus::Issued(IssueReason::Replaced(ServerCertProblem::NotTrusted(_)))
    ));
    let ca_pem = fs::read(fixture("standard").join("ca.pem")).unwrap();
    assert_eq!(read(dir.path(), TlsFile::CaCert), ca_pem);
}

#[test]
fn existing_openssl_credentials_load_without_changes() {
    for name in ["standard", "sec1_server_key"] {
        let dir = copy_fixture(name);
        let before = snapshot(dir.path());
        let tls = DeviceLinkTls::load_or_create(dir.path()).unwrap();
        assert!(!tls.created_ca(), "{name}");
        assert_eq!(tls.server_cert(), &ServerCertStatus::Loaded, "{name}");
        assert_eq!(snapshot(dir.path()), before, "{name}");
        let ca = read(dir.path(), TlsFile::CaCert);
        handshake(
            tls.server_config(),
            device_client(&ca, &rustls::version::TLS12),
        )
        .unwrap();
    }
}

#[test]
fn an_existing_ca_with_printable_names_issues_a_matching_server_certificate() {
    let dir = copy_fixture("printable_ca");
    let ca_pem = read(dir.path(), TlsFile::CaCert);
    let ca_key = read(dir.path(), TlsFile::CaKey);
    let tls = DeviceLinkTls::load_or_create(dir.path()).unwrap();
    assert!(!tls.created_ca());
    assert_eq!(
        tls.server_cert(),
        &ServerCertStatus::Issued(IssueReason::Missing)
    );
    assert_eq!(read(dir.path(), TlsFile::CaCert), ca_pem);
    assert_eq!(read(dir.path(), TlsFile::CaKey), ca_key);

    let ca_der = der(&ca_pem);
    let leaf_der = der(&read(dir.path(), TlsFile::ServerCert));
    let (_, ca) = parse_x509_certificate(&ca_der).unwrap();
    let (_, leaf) = parse_x509_certificate(&leaf_der).unwrap();
    assert_eq!(leaf.issuer().as_raw(), ca.subject().as_raw());
    let ca_key_id = ca
        .extensions()
        .iter()
        .find_map(|ext| match ext.parsed_extension() {
            ParsedExtension::SubjectKeyIdentifier(id) => Some(id.0),
            _ => None,
        });
    let leaf_authority = leaf
        .extensions()
        .iter()
        .find_map(|ext| match ext.parsed_extension() {
            ParsedExtension::AuthorityKeyIdentifier(aki) => {
                aki.key_identifier.as_ref().map(|id| id.0)
            }
            _ => None,
        });
    assert_eq!(leaf_authority, ca_key_id);
    handshake(
        tls.server_config(),
        device_client(&ca_pem, &rustls::version::TLS13),
    )
    .unwrap();
}

#[test]
fn a_missing_ca_certificate_beside_other_files_is_refused() {
    let dir = copy_fixture("standard");
    fs::remove_file(dir.path().join("ca.pem")).unwrap();
    let before = snapshot(dir.path());
    let error = DeviceLinkTls::load_or_create(dir.path()).unwrap_err();
    assert!(matches!(
        &error,
        TlsError::CaCertMissing { present } if present == &[TlsFile::CaKey, TlsFile::ServerCert, TlsFile::ServerKey]
    ));
    assert_eq!(snapshot(dir.path()), before);
}

#[test]
fn an_orphan_ca_key_is_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("ca.key"),
        fs::read(fixture("standard").join("ca.key")).unwrap(),
    )
    .unwrap();
    let error = DeviceLinkTls::load_or_create(dir.path()).unwrap_err();
    assert!(matches!(&error, TlsError::CaCertMissing { present } if present == &[TlsFile::CaKey]));
    assert_eq!(
        read(dir.path(), TlsFile::CaKey),
        fs::read(fixture("standard").join("ca.key")).unwrap()
    );
}

#[test]
fn issuing_without_the_ca_key_is_refused() {
    let dir = copy_fixture("standard");
    fs::remove_file(dir.path().join("ca.key")).unwrap();
    // A valid pair still loads without the CA key.
    let tls = DeviceLinkTls::load_or_create(dir.path()).unwrap();
    assert_eq!(tls.server_cert(), &ServerCertStatus::Loaded);

    fs::remove_file(dir.path().join("server.pem")).unwrap();
    let error = DeviceLinkTls::load_or_create(dir.path()).unwrap_err();
    assert!(matches!(
        error,
        TlsError::CaKeyMissing {
            reason: IssueReason::Missing
        }
    ));
}

#[test]
fn a_ca_key_of_another_ca_is_refused() {
    let dir = copy_fixture("standard");
    fs::copy(
        fixture("printable_ca").join("ca.key"),
        dir.path().join("ca.key"),
    )
    .unwrap();
    fs::remove_file(dir.path().join("server.key")).unwrap();
    let error = DeviceLinkTls::load_or_create(dir.path()).unwrap_err();
    assert!(matches!(error, TlsError::CaKeyMismatch), "{error:?}");
    assert!(!dir.path().join("server.key").exists());
}

#[test]
fn an_unreadable_ca_certificate_is_refused_and_kept() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("ca.pem"), "not a certificate\n").unwrap();
    let error = DeviceLinkTls::load_or_create(dir.path()).unwrap_err();
    assert!(matches!(error, TlsError::CaCertUnusable(_)), "{error:?}");
    assert_eq!(read(dir.path(), TlsFile::CaCert), b"not a certificate\n");
}
