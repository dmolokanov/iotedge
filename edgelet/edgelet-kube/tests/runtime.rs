// Copyright (c) Microsoft. All rights reserved.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::error::Error as StdError;

use futures::{future, Future, IntoFuture};
use hyper::body::Payload;
use hyper::client::{Client as HyperClient, HttpConnector};
use hyper::error::Error as HyperError;
use hyper::{header, Body, Method, Request, Response, StatusCode};
use maplit::btreemap;
use native_tls::TlsConnector;
use serde_json::json;
use typed_headers::{mime, ContentLength, ContentType, HeaderMapExt};
use url::Url;

use docker::models::{AuthConfig, ContainerCreateBody, HostConfig, Mount};
use edgelet_core::{AuthId, ModuleSpec};
use edgelet_core::{Authenticator, ModuleRuntime};
use edgelet_docker::DockerConfig;
use edgelet_kube::ErrorKind;
use edgelet_kube::{KubeModuleRuntime, KubeRuntimeData};
use edgelet_test_utils::{get_unused_tcp_port, run_tcp_server};
use kube_client::{Client as KubeClient, Config, Error, HttpClient, TokenSource};

use edgelet_kube::constants::*;

#[derive(Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
struct RequestPath(String);

#[derive(Clone, PartialEq, Eq, Hash)]
struct HttpMethod(Method);

impl Ord for HttpMethod {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_str().cmp(other.0.as_str())
    }
}

impl PartialOrd for HttpMethod {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

trait CloneableService: objekt::Clone {
    type ReqBody: Payload;
    type ResBody: Payload;
    type Error: Into<Box<dyn StdError + Send + Sync>>;
    type Future: Future<Item = Response<Self::ResBody>, Error = Self::Error>;

    fn call(&self, req: Request<Self::ReqBody>) -> Self::Future;
}

objekt::clone_trait_object!(CloneableService<
    ReqBody = Body,
    ResBody = Body,
    Error = HyperError,
    Future = ResponseFuture,
> + Send);

type ResponseFuture = Box<dyn Future<Item = Response<Body>, Error = HyperError> + Send>;
type RequestHandler = Box<
    dyn CloneableService<
            ReqBody = Body,
            ResBody = Body,
            Error = HyperError,
            Future = ResponseFuture,
        > + Send,
>;

impl<T, F> CloneableService for T
where
    T: Fn(Request<Body>) -> F + Clone,
    F: IntoFuture<Item = Response<Body>, Error = HyperError>,
{
    type ReqBody = Body;
    type ResBody = Body;
    type Error = F::Error;
    type Future = F::Future;

    fn call(&self, req: Request<Self::ReqBody>) -> Self::Future {
        (self)(req).into_future()
    }
}

fn make_req_dispatcher(
    dispatch_table: BTreeMap<(HttpMethod, RequestPath), RequestHandler>,
    default_handler: RequestHandler,
) -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    move |req: Request<Body>| {
        let key = (
            HttpMethod(req.method().clone()),
            RequestPath(req.uri().path().to_string()),
        );
        let handler = dispatch_table.get(&key).unwrap_or(&default_handler);

        Box::new(handler.call(req))
    }
}

macro_rules! routes {
    ($($method:ident $path:expr => $handler:expr),+ $(,)*) => ({
        btreemap! {
            $((HttpMethod(Method::$method), RequestPath(From::from($path))) => Box::new($handler) as RequestHandler,)*
        }
    });
}

fn not_found_handler(_: Request<Body>) -> ResponseFuture {
    let response = Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::default())
        .unwrap();

    Box::new(future::ok(response))
}

#[test]
fn authenticate_returns_none_when_no_auth_token_provided() {
    let port = get_unused_tcp_port();

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => unauthenticated_token_review_handler()
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let runtime = create_runtime(&format!("http://localhost:{}", port));

    let req = Request::default();

    let task = runtime.authenticate(&req);

    let mut runtime = tokio::runtime::current_thread::Runtime::new().unwrap();
    runtime.spawn(server);
    let auth_id = runtime.block_on(task).unwrap();

    assert_eq!(AuthId::None, auth_id)
}

#[test]
fn authenticate_returns_none_when_invalid_auth_header_provided() {
    let port = get_unused_tcp_port();

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => unauthenticated_token_review_handler()
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let runtime = create_runtime(&format!("http://localhost:{}", port));

    let mut req = Request::default();
    req.headers_mut()
        .insert(header::AUTHORIZATION, "BeErer token".parse().unwrap());

    let task = runtime.authenticate(&req);

    let mut runtime = tokio::runtime::current_thread::Runtime::new().unwrap();
    runtime.spawn(server);
    let auth_id = runtime.block_on(task).unwrap();

    assert_eq!(AuthId::None, auth_id)
}

#[test]
fn authenticate_returns_none_when_invalid_auth_token_provided() {
    let port = get_unused_tcp_port();

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => unauthenticated_token_review_handler()
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let runtime = create_runtime(&format!("http://localhost:{}", port));

    let mut req = Request::default();
    req.headers_mut().insert(
        header::AUTHORIZATION,
        "\u{3aa}\u{3a9}\u{3a4}".parse().unwrap(),
    );

    let task = runtime.authenticate(&req);

    let mut runtime = tokio::runtime::current_thread::Runtime::new().unwrap();
    runtime.spawn(server);
    let err = runtime.block_on(task).err().unwrap();

    assert_eq!(&ErrorKind::ModuleAuthenticationError, err.kind());
}

#[test]
fn authenticate_returns_none_when_unknown_auth_token_provided() {
    let port = get_unused_tcp_port();

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => unauthenticated_token_review_handler()
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let runtime = create_runtime(&format!("http://localhost:{}", port));

    let mut req = Request::default();
    req.headers_mut().insert(
        header::AUTHORIZATION,
        "Bearer token-unknown".parse().unwrap(),
    );

    let task = runtime.authenticate(&req);

    let mut runtime = tokio::runtime::current_thread::Runtime::new().unwrap();
    runtime.spawn(server);
    let auth_id = runtime.block_on(task).unwrap();

    assert_eq!(AuthId::None, auth_id)
}

#[test]
fn authenticate_returns_auth_id_when_module_auth_token_provided() {
    let port = get_unused_tcp_port();

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => authenticated_token_review_handler()
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let runtime = create_runtime(&format!("http://localhost:{}", port));

    let mut req = Request::default();
    req.headers_mut()
        .insert(header::AUTHORIZATION, "Bearer token".parse().unwrap());

    let task = runtime.authenticate(&req);

    let mut runtime = tokio::runtime::current_thread::Runtime::new().unwrap();
    runtime.spawn(server);
    let auth_id = runtime.block_on(task).unwrap();

    assert_eq!(AuthId::Value("module-abc".into()), auth_id);
}

fn create_runtime(
    url: &str,
) -> KubeModuleRuntime<TestTokenSource, HttpClient<HttpConnector, Body>> {
    let namespace = String::from("my-namespace");
    let iot_hub_hostname = String::from("iothostname");
    let device_id = String::from("my_device_id");
    let edge_hostname = String::from("edge-hostname");
    let proxy_image = String::from("proxy-image");
    let proxy_config_path = String::from("proxy-confg-path");
    let proxy_config_map_name = String::from("config-volume");
    let image_pull_policy = String::from("IfNotPresent");
    let service_account_name = String::from("iotedge");
    let workload_uri = Url::parse("http://localhost:35000").unwrap();
    let management_uri = Url::parse("http://localhost:35001").unwrap();

    let client = KubeClient::with_client(get_config(url), HttpClient(HyperClient::new()));
    //    let client:KubeClient<ValueToken, HttpClient<HttpsConnector<HttpConnector>,Body>>= KubeClient::new(get_config(url));

    KubeModuleRuntime::new(
        client,
        namespace.clone(),
        true,
        iot_hub_hostname.clone(),
        device_id.clone(),
        edge_hostname.clone(),
        proxy_image.clone(),
        proxy_config_path.clone(),
        proxy_config_map_name.clone(),
        image_pull_policy.clone(),
        service_account_name.clone(),
        workload_uri.clone(),
        management_uri.clone(),
    )
    .unwrap()
}

fn get_config(url: &str) -> Config<TestTokenSource> {
    Config::new(
        Url::parse(url).unwrap(),
        "/api".to_string(),
        TestTokenSource,
        TlsConnector::new().unwrap(),
    )
}

fn authenticated_token_review_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    make_token_review_handler(|| {
        json!({
            "kind": "TokenReview",
            "spec": { "token": "token" },
            "status": {
                "authenticated": true,
                "user": {
                    "username": "module-abc"
                }
            }}
        )
        .to_string()
    })
}

fn unauthenticated_token_review_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    make_token_review_handler(|| {
        json!({
            "kind": "TokenReview",
            "spec": { "token": "token" },
            "status": {
                "authenticated": false,
            }}
        )
        .to_string()
    })
}

fn make_token_review_handler(
    on_token_review: impl Fn() -> String + Clone + Send + 'static,
) -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    move |_| {
        let response = on_token_review();
        let response_len = response.len();

        let mut response = Response::new(response.into());
        response
            .headers_mut()
            .typed_insert(&ContentLength(response_len as u64));
        response
            .headers_mut()
            .typed_insert(&ContentType(mime::APPLICATION_JSON));
        Box::new(future::ok(response)) as ResponseFuture
    }
}

#[test]
fn create_module() {
    let port = get_unused_tcp_port();

    let runtime = create_runtime(&format!("http://localhost:{}", port));
    let module = create_module_spec();

    let dispatch_table = routes!(
        GET format!("/api/v1/namespaces/{}/serviceaccounts", runtime.namespace()) => empty_list_service_accounts_handler(),
        POST format!("/api/v1/namespaces/{}/serviceaccounts", runtime.namespace()) => create_service_account_handler(),
        PUT format!("/api/v1/namespaces/{}/serviceaccounts/{}", runtime.namespace(), EDGE_EDGE_AGENT_NAME) => replace_service_account_handler(),
        GET format!("/apis/apps/v1/namespaces/{}/deployments", runtime.namespace()) => empty_list_deployments_handler(),
        POST format!("/apis/apps/v1/namespaces/{}/deployments", runtime.namespace()) => create_deployment_handler(),
        PUT format!("/apis/apps/v1/namespaces/{}/deployments/{}", runtime.namespace(), EDGE_EDGE_AGENT_NAME) => replace_deployment_handler(),
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let task = runtime.create(module);

    let mut runtime = tokio::runtime::current_thread::Runtime::new().unwrap();
    runtime.spawn(server);
    runtime.block_on(task).unwrap();
}

fn create_module_spec() -> ModuleSpec<DockerConfig> {
    let create_body = ContainerCreateBody::new()
        .with_host_config(
            HostConfig::new()
                .with_binds(vec![String::from("/a:/b:ro"), String::from("/c:/d")])
                .with_privileged(true)
                .with_mounts(vec![
                    Mount::new()
                        .with__type(String::from("bind"))
                        .with_read_only(true)
                        .with_source(String::from("/e"))
                        .with_target(String::from("/f")),
                    Mount::new()
                        .with__type(String::from("bind"))
                        .with_source(String::from("/g"))
                        .with_target(String::from("/h")),
                    Mount::new()
                        .with__type(String::from("volume"))
                        .with_read_only(true)
                        .with_source(String::from("i"))
                        .with_target(String::from("/j")),
                    Mount::new()
                        .with__type(String::from("volume"))
                        .with_source(String::from("k"))
                        .with_target(String::from("/l")),
                ]),
        )
        .with_labels({
            let mut labels = HashMap::<String, String>::new();
            labels.insert(String::from("label1"), String::from("value1"));
            labels.insert(String::from("label2"), String::from("value2"));
            labels
        });
    let auth_config = AuthConfig::new()
        .with_password(String::from("a password"))
        .with_username(String::from("USERNAME"))
        .with_serveraddress(String::from("REGISTRY"));
    ModuleSpec::new(
        "$edgeAgent".to_string(),
        "docker".to_string(),
        DockerConfig::new("my-image:v1.0".to_string(), create_body, Some(auth_config)).unwrap(),
        {
            let mut env = HashMap::new();
            env.insert(String::from("a"), String::from("b"));
            env.insert(String::from("C"), String::from("D"));
            env
        },
    )
    .unwrap()
}

fn empty_list_service_accounts_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    move |_| {
        response(StatusCode::OK, || {
            json!({
                "kind": "ServiceAccountList",
                "apiVersion": "v1",
                "items": []
            })
            .to_string()
        })
    }
}

fn create_service_account_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    move |_| {
        response(StatusCode::CREATED, || {
            json!({
                "kind": "ServiceAccount",
                "apiVersion": "v1",
                "metadata": {
                    "name": "edgeagent",
                    "namespace": "my-namespace",
                }
            })
            .to_string()
        })
    }
}

fn replace_service_account_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    move |_| {
        response(StatusCode::OK, || {
            json!({
                "kind": "ServiceAccount",
                "apiVersion": "v1",
                "metadata": {
                    "name": "edgeagent",
                    "namespace": "my-namespace",
                }
            })
            .to_string()
        })
    }
}

fn empty_list_deployments_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    move |_| {
        response(StatusCode::OK, || {
            json!({
                "kind": "DeploymentList",
                "apiVersion": "apps/v1",
                "items": []
            })
            .to_string()
        })
    }
}

fn create_deployment_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    move |_| {
        response(StatusCode::CREATED, || {
            json!({
                "kind": "Deployment",
                "apiVersion": "apps/v1",
                "metadata": {
                    "name": "edgeagent",
                    "namespace": "my-namespace",
                }
            })
            .to_string()
        })
    }
}

fn replace_deployment_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    move |_| {
        response(StatusCode::OK, || {
            json!({
                "kind": "Deployment",
                "apiVersion": "apps/v1",
                "metadata": {
                    "name": "edgeagent",
                    "namespace": "msiot-dmolokan-iothub-dmolokan-edge-aks",
                },
            })
            .to_string()
        })
    }
}

fn response(
    status_code: StatusCode,
    response: impl Fn() -> String + Clone + Send + 'static,
) -> ResponseFuture {
    let response = response();
    let response_len = response.len();

    let mut response = Response::new(response.into());
    *response.status_mut() = StatusCode::CREATED;
    response
        .headers_mut()
        .typed_insert(&ContentLength(response_len as u64));
    response
        .headers_mut()
        .typed_insert(&ContentType(mime::APPLICATION_JSON));

    Box::new(future::ok(response)) as ResponseFuture
}

#[derive(Clone)]
struct TestTokenSource;

impl TokenSource for TestTokenSource {
    type Error = Error;

    fn get(&self) -> kube_client::error::Result<Option<String>> {
        Ok(None)
    }
}
