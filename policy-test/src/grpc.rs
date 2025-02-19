//! A gRPC client for the inbound policy API.
//!
//! This client currently discovers a destination controller pod via the k8s API and uses port
//! forwarding to connect to a running instance.

use anyhow::Result;
use linkerd2_proxy_api::inbound::inbound_server_policies_client::InboundServerPoliciesClient;
pub use linkerd2_proxy_api::*;
use linkerd_policy_controller_k8s_api::{self as k8s, ResourceExt};
use tokio::io;

#[macro_export]
macro_rules! assert_is_default_all_unauthenticated {
    ($config:expr) => {
        assert_eq!(
            $config.labels,
            vec![
                ("kind".to_string(), "default".to_string()),
                ("name".to_string(), "all-unauthenticated".to_string()),
            ]
            .into_iter()
            .collect()
        );
        assert_eq!($config.authorizations.len(), 1);
    };
}

#[macro_export]
macro_rules! assert_protocol_detect {
    ($config:expr) => {{
        use linkerd2_proxy_api::inbound;

        assert_eq!(
            $config.protocol,
            Some(inbound::ProxyProtocol {
                kind: Some(inbound::proxy_protocol::Kind::Detect(
                    inbound::proxy_protocol::Detect {
                        timeout: Some(time::Duration::from_secs(10).into()),
                    }
                )),
            }),
        );
    }};
}

#[derive(Debug)]
pub struct PolicyClient {
    client: InboundServerPoliciesClient<GrpcHttp>,
}

#[derive(Debug)]
struct GrpcHttp {
    tx: hyper::client::conn::SendRequest<tonic::body::BoxBody>,
}

// === impl PolicyClient ===

impl PolicyClient {
    pub async fn port_forwarded(client: &kube::Client) -> Self {
        let pod = Self::get_policy_controller_pod(client)
            .await
            .expect("failed to find a policy controller pod");
        let io = Self::connect_port_forward(client, &pod)
            .await
            .expect("failed to establish a port forward");
        let http = GrpcHttp::handshake(io)
            .await
            .expect("failed to connect to the gRPC server");
        PolicyClient {
            client: InboundServerPoliciesClient::new(http),
        }
    }

    pub async fn get_port(
        &mut self,
        ns: &str,
        pod: &str,
        port: u16,
    ) -> Result<inbound::Server, tonic::Status> {
        let rsp = self
            .client
            .get_port(tonic::Request::new(inbound::PortSpec {
                workload: format!("{}:{}", ns, pod),
                port: port as u32,
            }))
            .await?;
        Ok(rsp.into_inner())
    }

    pub async fn watch_port(
        &mut self,
        ns: &str,
        pod: &str,
        port: u16,
    ) -> Result<tonic::Streaming<inbound::Server>, tonic::Status> {
        let rsp = self
            .client
            .watch_port(tonic::Request::new(inbound::PortSpec {
                workload: format!("{}:{}", ns, pod),
                port: port as u32,
            }))
            .await?;
        Ok(rsp.into_inner())
    }

    async fn get_policy_controller_pod(client: &kube::Client) -> Result<String> {
        let params = kube::api::ListParams::default()
            .labels("linkerd.io/control-plane-component=destination");
        let mut pods = kube::Api::<k8s::Pod>::namespaced(client.clone(), "linkerd")
            .list(&params)
            .await?;
        let pod = pods
            .items
            .pop()
            .ok_or_else(|| anyhow::anyhow!("no destination controller pods found"))?;
        Ok(pod.name())
    }

    async fn connect_port_forward(
        client: &kube::Client,
        pod: &str,
    ) -> Result<impl io::AsyncRead + io::AsyncWrite + Unpin> {
        let mut pf = kube::Api::<k8s::Pod>::namespaced(client.clone(), "linkerd")
            .portforward(pod, &[8090])
            .await?;
        let io = pf.take_stream(8090).expect("must have a stream");
        Ok(io)
    }
}

// === impl GrpcHttp ===

impl GrpcHttp {
    async fn handshake<I>(io: I) -> Result<Self>
    where
        I: io::AsyncRead + io::AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, conn) = hyper::client::conn::Builder::new()
            .http2_only(true)
            .handshake(io)
            .await?;
        tokio::spawn(conn);
        Ok(Self { tx })
    }
}

impl hyper::service::Service<hyper::Request<tonic::body::BoxBody>> for GrpcHttp {
    type Response = hyper::Response<hyper::Body>;
    type Error = hyper::Error;
    type Future = hyper::client::conn::ResponseFuture;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.tx.poll_ready(cx)
    }

    fn call(&mut self, req: hyper::Request<tonic::body::BoxBody>) -> Self::Future {
        let (mut parts, body) = req.into_parts();

        let mut uri = parts.uri.into_parts();
        uri.scheme = Some(hyper::http::uri::Scheme::HTTP);
        uri.authority = Some(
            "linkerd-destination.linkerd.svc.cluster.local:8090"
                .parse()
                .unwrap(),
        );
        parts.uri = hyper::Uri::from_parts(uri).unwrap();

        self.tx.call(hyper::Request::from_parts(parts, body))
    }
}
