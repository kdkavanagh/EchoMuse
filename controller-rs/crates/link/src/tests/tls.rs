//! The TLS listener with credentials from the config crate, and a device that trusts
//! only the generated CA (spec §3.1.2).

use std::sync::Arc;

use echomuse_config::DeviceLinkTls;
use futures_util::SinkExt;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use tokio_tungstenite::tungstenite::Message;

use super::{Harness, Route, TestAssets, mic_packet, refused_status, test_config};
use crate::{Ingress, TlsListenerConfig, Transport};

fn client_trusting(ca_pem: &str) -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap())
        .unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

#[tokio::test]
async fn a_device_trusting_only_the_generated_ca_holds_a_session_over_tls() {
    let tls_dir = tempfile::tempdir().unwrap();
    let credentials = DeviceLinkTls::load_or_create(tls_dir.path()).unwrap();
    assert!(credentials.created_ca());
    let mut config = test_config();
    config.require_device_tls = true;
    config.tls = Some(TlsListenerConfig {
        port: 0,
        server_config: credentials.server_config(),
    });
    let mut h = Harness::with(config, TestAssets::default()).await;
    let route = Route::Tls(client_trusting(credentials.ca_pem()));

    let device = h.approved("G0TLS00001").await;
    let (control, mut admitted) = device.connect(&mut h, &route).await;
    assert_eq!(admitted.request.transport, Transport::Tls);
    assert_eq!(admitted.session.link.transport(), Transport::Tls);

    let session = control.session().to_owned();
    let mut audio = device
        .dial_joined(&h, &route, crate::PATH_AUDIO, &session)
        .await
        .unwrap();
    audio
        .send(Message::Binary(mic_packet(5, 0, 0)))
        .await
        .unwrap();
    let item = admitted.session.ingress.recv().await.unwrap();
    assert!(matches!(item.ingress, Ingress::Audio(_)));

    // The plain listener cannot join a TLS session.
    let plain = device
        .dial_joined(&h, &Route::Tcp, crate::PATH_ASSETS, &session)
        .await;
    assert_eq!(refused_status(plain), 403);
}
