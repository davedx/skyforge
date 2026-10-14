//! Per-instance loopback IPC. The GUI owns all handoff persistence and agent startup.
use eframe::egui;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::Duration,
};

// Accommodate the bounded brief even when every character requires JSON escaping.
const MAX_MESSAGE: u64 = 2 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Serialize, Deserialize)]
pub struct Endpoint {
    pub address: SocketAddr,
    pub token: String,
}
impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireRequest {
    token: String,
    source: String,
    turn: String,
    args: Value,
}

pub struct Request {
    pub source: String,
    pub turn: String,
    pub args: Value,
    pub reply: mpsc::Sender<Result<String, String>>,
}

pub struct Host {
    endpoint: Endpoint,
    requests: mpsc::Receiver<Request>,
    stop: Arc<AtomicBool>,
    listener: Option<thread::JoinHandle<()>>,
}

impl Host {
    pub fn new(ctx: egui::Context) -> Result<Self, String> {
        let listener =
            TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(|e| e.to_string())?;
        let endpoint = Endpoint {
            address: listener.local_addr().map_err(|e| e.to_string())?,
            token: uuid::Uuid::new_v4().to_string(),
        };
        let token = endpoint.token.clone();
        let (tx, requests) = mpsc::sync_channel(32);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let listener = thread::spawn(move || {
            for stream in listener.incoming() {
                if stopping.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else {
                    break;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                let result = (|| {
                    let request: WireRequest = read_json(&stream)?;
                    if request.token != token {
                        return Err("Invalid Skyforge handoff capability.".into());
                    }
                    let (reply, response) = mpsc::channel();
                    tx.try_send(Request {
                        source: request.source,
                        turn: request.turn,
                        args: request.args,
                        reply,
                    })
                    .map_err(|_| {
                        "Skyforge cannot accept a handoff right now. Retry the same brief."
                            .to_owned()
                    })?;
                    ctx.request_repaint();
                    let deadline = std::time::Instant::now() + TIMEOUT;
                    loop {
                        if stopping.load(Ordering::SeqCst) {
                            return Err("Skyforge is shutting down. Retry the same brief after reopening it.".into());
                        }
                        match response.recv_timeout(Duration::from_millis(100)) {
                            Ok(result) => return result,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return Err("Skyforge closed the handoff request. Retry the same brief.".into()),
                            Err(mpsc::RecvTimeoutError::Timeout) if std::time::Instant::now() >= deadline => return Err("Skyforge did not respond in time. The handoff may already exist; retry the same brief.".into()),
                            Err(_) => {},
                        }
                    }
                })();
                let _ = write_json(&mut stream, &result);
            }
        });
        Ok(Self {
            endpoint,
            requests,
            stop,
            listener: Some(listener),
        })
    }

    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }
    pub fn try_recv(&self) -> Result<Request, mpsc::TryRecvError> {
        self.requests.try_recv()
    }
    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake a blocking accept. An in-flight request notices stop within 100 ms.
        let _ = TcpStream::connect_timeout(&self.endpoint.address, Duration::from_millis(100));
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn call(endpoint: &Endpoint, source: &str, turn: &str, args: &Value) -> Result<String, String> {
    if !endpoint.address.ip().is_loopback() {
        return Err("Skyforge handoff IPC must be local.".into());
    }
    let mut stream = TcpStream::connect_timeout(&endpoint.address, Duration::from_secs(2))
        .map_err(|e| format!("Cannot reach the Skyforge host: {e}. Reopen Skyforge and send a new Thinker request."))?;
    stream
        .set_read_timeout(Some(TIMEOUT + Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .map_err(|e| e.to_string())?;
    write_json(
        &mut stream,
        &WireRequest {
            token: endpoint.token.clone(),
            source: source.into(),
            turn: turn.into(),
            args: args.clone(),
        },
    )?;
    read_json::<Result<String,String>>(&stream)
        .map_err(|e| format!("Skyforge handoff response was lost: {e}. Retry the same brief; it may already have been accepted."))?
}

fn read_json<T: serde::de::DeserializeOwned>(stream: &TcpStream) -> Result<T, String> {
    let mut bytes = Vec::new();
    BufReader::new(stream.take(MAX_MESSAGE + 1))
        .read_until(b'\n', &mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_MESSAGE || bytes.last() != Some(&b'\n') {
        return Err("Invalid or oversized Skyforge IPC message.".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}
fn write_json<T: Serialize>(stream: &mut TcpStream, value: &T) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_MESSAGE {
        return Err("Skyforge IPC message is too large.".into());
    }
    stream.write_all(&bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receive(host: &Host) -> Request {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match host.try_recv() {
                Ok(request) => return request,
                Err(_) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(1))
                }
                Err(_) => panic!("IPC request did not reach host"),
            }
        }
    }

    #[test]
    fn request_wakes_gui_and_returns_host_response() {
        let ctx = egui::Context::default();
        let wakes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callback_wakes = wakes.clone();
        ctx.set_request_repaint_callback(move |_| {
            callback_wakes.fetch_add(1, Ordering::SeqCst);
        });
        let host = Host::new(ctx).unwrap();
        let endpoint = host.endpoint();
        let client = thread::spawn(move || {
            call(
                &endpoint,
                "trusted-source",
                "trusted-turn",
                &serde_json::json!({"task":"Do work"}),
            )
        });
        let request = receive(&host);
        assert_eq!(request.source, "trusted-source");
        assert_eq!(request.turn, "trusted-turn");
        assert_eq!(request.args["task"], "Do work");
        request
            .reply
            .send(Ok("durable Builder result".into()))
            .unwrap();
        assert_eq!(client.join().unwrap().unwrap(), "durable Builder result");
        assert!(wakes.load(Ordering::SeqCst) > 0);
    }

    #[test]
    fn rejects_wrong_capability_nonlocal_address_and_oversized_messages() {
        let host = Host::new(egui::Context::default()).unwrap();
        let mut endpoint = host.endpoint();
        endpoint.token = "wrong".into();
        assert!(call(&endpoint, "source", "turn", &serde_json::json!({}))
            .unwrap_err()
            .contains("capability"));
        assert!(host.try_recv().is_err());
        endpoint.address = "192.0.2.1:1234".parse().unwrap();
        assert!(call(&endpoint, "source", "turn", &serde_json::json!({}))
            .unwrap_err()
            .contains("must be local"));
        assert!(call(
            &host.endpoint(),
            "source",
            "turn",
            &serde_json::json!({"task":"x".repeat(MAX_MESSAGE as usize)})
        )
        .unwrap_err()
        .contains("too large"));
        let mut stream = TcpStream::connect(host.endpoint().address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream.write_all(b"{malformed}\n").unwrap();
        assert!(read_json::<Result<String, String>>(&stream)
            .unwrap()
            .is_err());
        assert!(host.try_recv().is_err());
        assert!(!format!("{:?}", host.endpoint()).contains(&host.endpoint().token));
    }

    #[test]
    fn dropped_response_does_not_prevent_next_request() {
        let host = Host::new(egui::Context::default()).unwrap();
        let endpoint = host.endpoint();
        let mut stream = TcpStream::connect(endpoint.address).unwrap();
        write_json(
            &mut stream,
            &WireRequest {
                token: endpoint.token.clone(),
                source: "source".into(),
                turn: "turn".into(),
                args: serde_json::json!({}),
            },
        )
        .unwrap();
        drop(stream);
        let accepted = receive(&host);
        accepted.reply.send(Ok("already persisted".into())).unwrap();
        let client =
            thread::spawn(move || call(&endpoint, "source", "turn", &serde_json::json!({})));
        let retry = receive(&host);
        assert_eq!(
            (retry.source, retry.turn, retry.args),
            (accepted.source, accepted.turn, accepted.args)
        );
        retry.reply.send(Ok("already persisted".into())).unwrap();
        assert_eq!(client.join().unwrap().unwrap(), "already persisted");
    }

    #[test]
    fn shutdown_releases_pending_call_and_stale_endpoint_cannot_reconnect() {
        let mut host = Host::new(egui::Context::default()).unwrap();
        let endpoint = host.endpoint();
        let client_endpoint = endpoint.clone();
        let client =
            thread::spawn(move || call(&client_endpoint, "source", "turn", &serde_json::json!({})));
        let _pending = receive(&host);
        let start = std::time::Instant::now();
        host.shutdown();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(client
            .join()
            .unwrap()
            .unwrap_err()
            .contains("shutting down"));
        assert!(call(&endpoint, "source", "turn", &serde_json::json!({}))
            .unwrap_err()
            .contains("Cannot reach"));
        let mut w = crate::store::Workstream::new(std::path::Path::new("/tmp"));
        w.delegation_endpoint = Some(endpoint.clone());
        assert!(!serde_json::to_string(&w).unwrap().contains(&endpoint.token));
    }
}
