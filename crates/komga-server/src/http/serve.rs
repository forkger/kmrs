//! The accept loop: one task per connection, served by hyper as HTTP/1.
//!
//! This stands in for `axum::serve`, which takes no connection settings. It keeps
//! what that function does for this server: accept errors do not end the loop, the
//! handlers see the peer address as `ConnectInfo`, and a shutdown stops accepting,
//! lets every connection finish the request it is in and then waits for all of them.
//! HTTP upgrades are not passed on; nothing in the server uses them.

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::pin;
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::Router;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;

/// Serves `router` on `listener` until `shutdown` completes, then returns once
/// every open connection has ended.
pub async fn serve(listener: TcpListener, router: Router, shutdown: impl Future<Output = ()>) {
    let mut http = http1::Builder::new();
    // hyper's own default only applies once a timer is set; no limit is stated here
    // so that none can appear by accident
    http.header_read_timeout(None::<Duration>);
    let connections = GracefulShutdown::new();
    let mut shutdown = pin!(shutdown);

    loop {
        let (socket, remote_addr) = tokio::select! {
            accepted = accept(&listener) => accepted,
            _ = &mut shutdown => break,
        };
        let router = router.clone();
        let service = service_fn(move |request: axum::http::Request<Incoming>| {
            let mut request = request.map(Body::new);
            request.extensions_mut().insert(ConnectInfo(remote_addr));
            router.clone().oneshot(request)
        });
        let connection = connections.watch(http.serve_connection(TokioIo::new(socket), service));
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::trace!("connection from {remote_addr} ended with an error: {e}");
            }
        });
    }

    // closed before the wait, so a client connecting during it is refused
    // instead of left unanswered in the backlog
    drop(listener);
    connections.shutdown().await;
}

/// Waits for the next connection. An error does not end the loop: one that belongs
/// to the connection being accepted is skipped, and anything else (running out of
/// file descriptors is the usual one) is logged and retried after a pause, because
/// it tends to clear once other connections close.
async fn accept(listener: &TcpListener) -> (TcpStream, SocketAddr) {
    loop {
        match listener.accept().await {
            Ok(accepted) => return accepted,
            Err(e) if is_connection_error(&e) => {}
            Err(e) => {
                tracing::error!("accept error: {e}");
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::watch;
    use tokio::task::JoinHandle;

    /// Long enough for a loaded machine, short enough that a connection the server
    /// never lets go of fails the test instead of hanging the run.
    const PATIENCE: Duration = Duration::from_secs(10);

    struct Server {
        addr: SocketAddr,
        shutdown: watch::Sender<bool>,
        served: JoinHandle<()>,
    }

    impl Server {
        /// Starts the loop on a free port. `router` gets the shutdown channel so a
        /// handler can end a stream on it, as the event streams of the server do.
        async fn start(router: impl FnOnce(watch::Receiver<bool>) -> Router) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (shutdown, shutdown_rx) = watch::channel(false);
            let router = router(shutdown_rx.clone());
            let served = tokio::spawn(serve(listener, router, async move {
                let mut shutdown_rx = shutdown_rx;
                let _ = shutdown_rx.wait_for(|stop| *stop).await;
            }));
            Self {
                addr,
                shutdown,
                served,
            }
        }

        async fn connect(&self) -> TcpStream {
            TcpStream::connect(self.addr).await.unwrap()
        }

        fn shut_down(&self) {
            self.shutdown.send_replace(true);
        }

        async fn stopped(self) {
            tokio::time::timeout(PATIENCE, self.served)
                .await
                .expect("serve returns after the shutdown")
                .unwrap();
        }
    }

    fn plain(router: Router) -> impl FnOnce(watch::Receiver<bool>) -> Router {
        move |_| router
    }

    async fn send(stream: &mut TcpStream, request: &str) {
        stream.write_all(request.as_bytes()).await.unwrap();
    }

    const GET: &str = "GET / HTTP/1.1\r\nHost: test\r\n\r\n";

    /// Reads one response with a `content-length` and returns its status line and body.
    async fn response(stream: &mut TcpStream) -> (String, String) {
        tokio::time::timeout(PATIENCE, async {
            let mut received = Vec::new();
            let head_end = loop {
                if let Some(at) = received.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
                let mut chunk = [0u8; 1024];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed before a response head");
                received.extend_from_slice(&chunk[..n]);
            };
            let head = String::from_utf8(received[..head_end].to_vec()).unwrap();
            let length: usize = head
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .expect("a content-length");
            while received.len() < head_end + length {
                let mut chunk = [0u8; 1024];
                let n = stream.read(&mut chunk).await.unwrap();
                assert!(n > 0, "connection closed inside a response body");
                received.extend_from_slice(&chunk[..n]);
            }
            let status = head.lines().next().unwrap().to_string();
            let body = String::from_utf8(received[head_end..head_end + length].to_vec()).unwrap();
            (status, body)
        })
        .await
        .expect("a response in time")
    }

    /// Everything the server still sends until it closes the connection.
    async fn rest(stream: &mut TcpStream) -> String {
        let mut received = Vec::new();
        tokio::time::timeout(PATIENCE, stream.read_to_end(&mut received))
            .await
            .expect("the server closes the connection")
            .unwrap();
        String::from_utf8(received).unwrap()
    }

    fn peer_address() -> Router {
        Router::new().route(
            "/",
            get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move { peer.to_string() }),
        )
    }

    #[tokio::test]
    async fn a_handler_sees_the_address_of_the_client() {
        let server = Server::start(plain(peer_address())).await;
        let mut client = server.connect().await;

        send(&mut client, GET).await;
        let (status, body) = response(&mut client).await;

        assert_eq!(status, "HTTP/1.1 200 OK");
        assert_eq!(body, client.local_addr().unwrap().to_string());
    }

    #[tokio::test]
    async fn a_connection_is_kept_for_the_next_request() {
        let server = Server::start(plain(peer_address())).await;
        let mut client = server.connect().await;

        send(&mut client, GET).await;
        let first = response(&mut client).await;
        send(&mut client, GET).await;
        let second = response(&mut client).await;

        assert_eq!(first.0, "HTTP/1.1 200 OK");
        assert_eq!(second, first);
    }

    #[tokio::test]
    async fn a_shutdown_closes_idle_connections() {
        let server = Server::start(plain(peer_address())).await;
        let mut used = server.connect().await;
        send(&mut used, GET).await;
        response(&mut used).await;
        let mut unused = server.connect().await;
        // time for the loop to accept the second connection: one still waiting
        // in the backlog would be reset by the shutdown, not closed
        tokio::time::sleep(Duration::from_millis(200)).await;

        server.shut_down();

        assert_eq!(rest(&mut used).await, "");
        assert_eq!(rest(&mut unused).await, "");
        server.stopped().await;
    }

    #[tokio::test]
    async fn a_request_for_an_upgrade_is_answered_as_a_plain_request() {
        let server = Server::start(plain(peer_address())).await;
        let mut client = server.connect().await;

        send(
            &mut client,
            "GET / HTTP/1.1\r\nHost: test\r\nConnection: upgrade\r\nUpgrade: websocket\r\n\r\n",
        )
        .await;
        let upgrade = response(&mut client).await;
        send(&mut client, GET).await;
        let next = response(&mut client).await;

        assert_eq!(upgrade.0, "HTTP/1.1 200 OK");
        assert_eq!(next, upgrade);
    }

    #[tokio::test]
    async fn a_shutdown_waits_for_a_request_in_progress_and_refuses_new_connections() {
        let (entered_tx, mut entered) = tokio::sync::mpsc::channel::<()>(1);
        let (release, released) = watch::channel(false);
        let router = Router::new().route(
            "/",
            get(move || {
                let entered_tx = entered_tx.clone();
                let mut released = released.clone();
                async move {
                    entered_tx.send(()).await.unwrap();
                    let _ = released.wait_for(|go| *go).await;
                    "finished"
                }
            }),
        );
        let server = Server::start(plain(router)).await;
        let mut client = server.connect().await;
        send(&mut client, GET).await;
        tokio::time::timeout(PATIENCE, entered.recv())
            .await
            .expect("the handler starts");

        server.shut_down();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !server.served.is_finished(),
            "serve returned while a request was in progress"
        );
        assert!(
            TcpStream::connect(server.addr).await.is_err(),
            "a connection was taken during the wait"
        );
        release.send_replace(true);

        let (status, body) = response(&mut client).await;
        assert_eq!(status, "HTTP/1.1 200 OK");
        assert_eq!(body, "finished");
        assert_eq!(rest(&mut client).await, "");
        server.stopped().await;
    }

    #[tokio::test]
    async fn a_stream_that_ends_on_the_shutdown_lets_serve_return() {
        let server = Server::start(|shutdown| {
            Router::new().route(
                "/",
                get(move || {
                    let shutdown = shutdown.clone();
                    async move {
                        let parts = futures_util::stream::unfold(
                            (shutdown, 0u8),
                            |(mut shutdown, sent)| async move {
                                match sent {
                                    0 => Some((Ok::<_, io::Error>("open "), (shutdown, 1))),
                                    1 => {
                                        let _ = shutdown.wait_for(|stop| *stop).await;
                                        Some((Ok("closing"), (shutdown, 2)))
                                    }
                                    _ => None,
                                }
                            },
                        );
                        Body::from_stream(parts)
                    }
                }),
            )
        })
        .await;
        let mut client = server.connect().await;
        send(&mut client, GET).await;
        let mut first = [0u8; 1024];
        let n = tokio::time::timeout(PATIENCE, client.read(&mut first))
            .await
            .expect("the stream starts")
            .unwrap();
        assert!(String::from_utf8_lossy(&first[..n]).starts_with("HTTP/1.1 200 OK"));

        server.shut_down();

        let remainder = rest(&mut client).await;
        assert!(remainder.contains("closing"), "got {remainder:?}");
        assert!(remainder.ends_with("0\r\n\r\n"), "got {remainder:?}");
        server.stopped().await;
    }
}
