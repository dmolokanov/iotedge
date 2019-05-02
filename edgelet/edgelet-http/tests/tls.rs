#![cfg(not(windows))]
#![deny(rust_2018_idioms, warnings)]
#![deny(clippy::all, clippy::pedantic)]

use std::env;

use edgelet_core::crypto::CreateCertificate;
use edgelet_core::{CertificateIssuer, CertificateProperties, CertificateType, IOTEDGED_CA_ALIAS};
use edgelet_hsm::Crypto;
use edgelet_http::certificate_manager::CertificateManager;
use edgelet_http::route::{Builder, Parameters, RegexRoutesBuilder, Router};
use edgelet_http::Error as HttpError;
use edgelet_http::HyperExt;
use edgelet_http::{Run, Version};
use edgelet_test_utils::get_unused_tcp_port;

use futures::{future, Future};
use hyper::server::conn::Http;
use hyper::{Body, Request, Response, StatusCode};
use hyper_tls;
use native_tls::TlsConnector;
use tempdir::TempDir;
use url::Url;

const HOMEDIR_KEY: &str = "IOTEDGE_HOMEDIR";

#[test]
#[cfg_attr(target_os = "macos", ignore)] // TODO: remove when macOS security framework supports opening pcks12 file with empty password
fn tls_functional_test() {
    let mut http = hyper::client::HttpConnector::new(1);
    http.enforce_http(false);

    let mut tls_connector_builder = TlsConnector::builder();

    // This is because are using a self signed cert
    tls_connector_builder.danger_accept_invalid_certs(true);

    let tls_connector = tls_connector_builder.build().unwrap();

    let https_connector =
        hyper_tls::HttpsConnector::<hyper::client::HttpConnector>::from((http, tls_connector));

    let client = hyper::Client::builder().build::<_, hyper::Body>(https_connector);

    let port = get_unused_tcp_port();
    let addr = format!("https://localhost:{}", port);
    let server = configure_test(&addr).map_err(|err| eprintln!("{}", err));

    let mut runtime = tokio::runtime::current_thread::Runtime::new().unwrap();
    runtime.spawn(server);

    let full_address = format!("{}/route1/hello?api-version=2018-06-28", addr);

    let request = client.get(full_address.parse().unwrap());

    let res = runtime.block_on(request).unwrap();
    assert_eq!(res.status(), 200);
}

pub fn configure_test(address: &str) -> Run {
    // setup the IOTEDGE_HOMEDIR folder where certs can be generated and stored
    let home_dir = TempDir::new("tls_integration_test").unwrap();
    env::set_var(HOMEDIR_KEY, &home_dir.path());
    println!("IOTEDGE_HOMEDIR set to {:#?}", home_dir.path());

    let crypto = Crypto::new().unwrap();

    // create the default issuing CA cert properties
    let edgelet_ca_props = CertificateProperties::new(
        3600,
        "test-iotedge-cn".to_string(),
        CertificateType::Ca,
        IOTEDGED_CA_ALIAS.to_string(),
    )
    .with_issuer(CertificateIssuer::DeviceCa);

    let _workload_ca_cert = crypto.create_certificate(&edgelet_ca_props).unwrap();

    let edgelet_cert_props = CertificateProperties::new(
        3600,
        "testtls".to_string(),
        CertificateType::Server,
        "localhost".to_string(),
    )
    .with_issuer(CertificateIssuer::DeviceCa);

    let manager = CertificateManager::new(crypto.clone(), edgelet_cert_props).unwrap();

    let recognizer = RegexRoutesBuilder::default()
        .get(Version::Version2018_06_28, "/route1/hello", route1)
        .finish();
    let router = Router::from(recognizer);

    Http::new()
        .bind_url(Url::parse(address).unwrap(), router, Some(&manager))
        .unwrap()
        .run()
}

#[allow(clippy::needless_pass_by_value)]
fn route1(
    _req: Request<Body>,
    _params: Parameters,
) -> Box<dyn Future<Item = Response<Body>, Error = HttpError> + Send> {
    let response = Response::builder()
        .status(StatusCode::OK)
        .body("Hello World!".into())
        .unwrap();
    Box::new(future::ok(response))
}
