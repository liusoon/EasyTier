use std::{net::SocketAddr, sync::Arc};

use anyhow::Context;
use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio_rustls::TlsAcceptor;
use tokio_websockets::{ClientBuilder, Limits, Message};
use zerocopy::AsBytes;

use crate::{rpc::TunnelInfo, tunnel::insecure_tls::get_insecure_tls_client_config};

use super::{
    common::{setup_sokcet2, TunnelWrapper},
    insecure_tls::{get_insecure_tls_cert, init_crypto_provider},
    packet_def::{ZCPacket, ZCPacketType},
    FromUrl, IpVersion, Tunnel, TunnelConnector, TunnelError, TunnelListener,
};

fn is_wss(addr: &url::Url) -> Result<bool, TunnelError> {
    match addr.scheme() {
        "ws" => Ok(false),
        "wss" => Ok(true),
        _ => Err(TunnelError::InvalidProtocol(addr.scheme().to_string())),
    }
}

async fn sink_from_zc_packet<E>(msg: ZCPacket) -> Result<Message, E> {
    Ok(Message::binary(msg.tunnel_payload_bytes().freeze()))
}

async fn map_from_ws_message(
    msg: Result<Message, tokio_websockets::Error>,
) -> Option<Result<ZCPacket, TunnelError>> {
    if msg.is_err() {
        tracing::error!(?msg, "recv from websocket error");
        return Some(Err(TunnelError::WebSocketError(msg.unwrap_err())));
    }

    let msg = msg.unwrap();
    if msg.is_close() {
        tracing::warn!("recv close message from websocket");
        return None;
    }

    if !msg.is_binary() {
        let msg = format!("{:?}", msg);
        tracing::error!(?msg, "Invalid packet");
        return Some(Err(TunnelError::InvalidPacket(msg)));
    }

    Some(Ok(ZCPacket::new_from_buf(
        BytesMut::from(msg.into_payload().as_bytes()),
        ZCPacketType::DummyTunnel,
    )))
}

#[derive(Debug)]
pub struct WSTunnelListener {
    addr: url::Url,
    listener: Option<TcpListener>,
}

impl WSTunnelListener {
    pub fn new(addr: url::Url) -> Self {
        WSTunnelListener {
            addr,
            listener: None,
        }
    }

    async fn try_accept(&mut self, stream: TcpStream) -> Result<Box<dyn Tunnel>, TunnelError> {
        let info = TunnelInfo {
            tunnel_type: self.addr.scheme().to_owned(),
            local_addr: self.local_url().into(),
            remote_addr: super::build_url_from_socket_addr(
                &stream.peer_addr()?.to_string(),
                self.addr.scheme().to_string().as_str(),
            )
            .into(),
        };

        let server_bulder = tokio_websockets::ServerBuilder::new().limits(Limits::unlimited());

        let ret: Box<dyn Tunnel> = if is_wss(&self.addr)? {
            init_crypto_provider();
            let (certs, key) = get_insecure_tls_cert();
            let config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .with_context(|| "Failed to create server config")?;
            let acceptor = TlsAcceptor::from(Arc::new(config));

            let stream = acceptor.accept(stream).await?;
            let (write, read) = server_bulder.accept(stream).await?.split();

            Box::new(TunnelWrapper::new(
                read.filter_map(move |msg| map_from_ws_message(msg)),
                write.with(move |msg| sink_from_zc_packet(msg)),
                Some(info),
            ))
        } else {
            let (write, read) = server_bulder.accept(stream).await?.split();
            Box::new(TunnelWrapper::new(
                read.filter_map(move |msg| map_from_ws_message(msg)),
                write.with(move |msg| sink_from_zc_packet(msg)),
                Some(info),
            ))
        };

        Ok(ret)
    }
}

#[async_trait::async_trait]
impl TunnelListener for WSTunnelListener {
    async fn listen(&mut self) -> Result<(), TunnelError> {
        let addr = SocketAddr::from_url(self.addr.clone(), IpVersion::Both)?;
        let socket2_socket = socket2::Socket::new(
            socket2::Domain::for_address(addr),
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )?;
        setup_sokcet2(&socket2_socket, &addr)?;
        let socket = TcpSocket::from_std_stream(socket2_socket.into());

        self.addr
            .set_port(Some(socket.local_addr()?.port()))
            .unwrap();

        self.listener = Some(socket.listen(1024)?);
        Ok(())
    }

    async fn accept(&mut self) -> Result<Box<dyn Tunnel>, super::TunnelError> {
        loop {
            let listener = self.listener.as_ref().unwrap();
            // only fail on tcp accept error
            let (stream, _) = listener.accept().await?;
            stream.set_nodelay(true).unwrap();
            match self.try_accept(stream).await {
                Ok(tunnel) => return Ok(tunnel),
                Err(e) => {
                    tracing::error!(?e, ?self, "Failed to accept ws/wss tunnel");
                    continue;
                }
            }
        }
    }

    fn local_url(&self) -> url::Url {
        self.addr.clone()
    }
}

pub struct WSTunnelConnector {
    addr: url::Url,
    ip_version: IpVersion,
}

impl WSTunnelConnector {
    pub fn new(addr: url::Url) -> Self {
        WSTunnelConnector {
            addr,
            ip_version: IpVersion::Both,
        }
    }
}

#[async_trait::async_trait]
impl TunnelConnector for WSTunnelConnector {
    async fn connect(&mut self) -> Result<Box<dyn Tunnel>, super::TunnelError> {
        let is_wss = is_wss(&self.addr)?;
        let addr = SocketAddr::from_url(self.addr.clone(), self.ip_version)?;
        let local_addr = if addr.is_ipv4() {
            "0.0.0.0:0"
        } else {
            "[::]:0"
        };

        let info = TunnelInfo {
            tunnel_type: self.addr.scheme().to_owned(),
            local_addr: super::build_url_from_socket_addr(
                &local_addr.to_string(),
                self.addr.scheme().to_string().as_str(),
            )
            .into(),
            remote_addr: self.addr.to_string(),
        };

        let connector =
            tokio_websockets::Connector::Rustls(Arc::new(get_insecure_tls_client_config()).into());
        let mut client_builder =
            ClientBuilder::from_uri(http::Uri::try_from(self.addr.to_string()).unwrap());
        if is_wss {
            init_crypto_provider();
            client_builder = client_builder.connector(&connector);
        }

        let (client, _) = client_builder.connect().await?;

        let (write, read) = client.split();
        let read = read.filter_map(move |msg| map_from_ws_message(msg));
        let write = write.with(move |msg| sink_from_zc_packet(msg));

        Ok(Box::new(TunnelWrapper::new(read, write, Some(info))))
    }

    fn remote_url(&self) -> url::Url {
        self.addr.clone()
    }

    fn set_ip_version(&mut self, ip_version: IpVersion) {
        self.ip_version = ip_version;
    }
}

#[cfg(test)]
pub mod tests {
    use crate::tunnel::common::tests::_tunnel_pingpong;
    use crate::tunnel::websocket::{WSTunnelConnector, WSTunnelListener};
    use crate::tunnel::{TunnelConnector, TunnelListener};

    #[rstest::rstest]
    #[tokio::test]
    #[serial_test::serial]
    async fn ws_pingpong(#[values("ws", "wss")] proto: &str) {
        let listener = WSTunnelListener::new(format!("{}://0.0.0.0:25556", proto).parse().unwrap());
        let connector =
            WSTunnelConnector::new(format!("{}://127.0.0.1:25556", proto).parse().unwrap());
        _tunnel_pingpong(listener, connector).await
    }

    // TODO: tokio-websockets cannot correctly handle close, benchmark case is disabled
    // #[rstest::rstest]
    // #[tokio::test]
    // #[serial_test::serial]
    // async fn ws_bench(#[values("ws", "wss")] proto: &str) {
    //     enable_log();
    //     let listener = WSTunnelListener::new(format!("{}://0.0.0.0:25557", proto).parse().unwrap());
    //     let connector =
    //         WSTunnelConnector::new(format!("{}://127.0.0.1:25557", proto).parse().unwrap());
    //     _tunnel_bench(listener, connector).await
    // }

    #[tokio::test]
    async fn ws_accept_wss() {
        let mut listener = WSTunnelListener::new("wss://0.0.0.0:25558".parse().unwrap());
        listener.listen().await.unwrap();
        let j = tokio::spawn(async move {
            let _ = listener.accept().await;
        });

        let mut connector = WSTunnelConnector::new("ws://127.0.0.1:25558".parse().unwrap());
        connector.connect().await.unwrap_err();

        let mut connector = WSTunnelConnector::new("wss://127.0.0.1:25558".parse().unwrap());
        connector.connect().await.unwrap();

        j.abort();
    }
}
