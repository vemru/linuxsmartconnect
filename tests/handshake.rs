//! Plays the phone's side of QR pairing against an in-process daemon: parse the QR,
//! pin the cert the way `so5.checkServerTrusted` does, upgrade `/readyfor/`, send the
//! MANAGER hello, and check the verify reply.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use tokio_rustls::rustls::crypto::{self, CryptoProvider};
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use tokio_rustls::rustls::{self, ClientConfig, DigitallySignedStruct, SignatureScheme};
use tokio_tungstenite::tungstenite::Message;

use smartconnect::identity::Identity;
use smartconnect::invite::{Invite, QR_PREFIX};
use smartconnect::pairing::PairingStore;
use smartconnect::server::{self, Daemon, QrOutput};

/// The phone's pin: lowercase hex SHA-256 of the leaf DER must equal the
/// base64-decoded `fp` from the QR. Nothing else about the chain is checked.
#[derive(Debug)]
struct FingerprintPin {
    expected_hex: String,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for FingerprintPin {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let actual = hex::encode(Sha256::digest(end_entity.as_ref()));
        if actual.eq_ignore_ascii_case(&self.expected_hex) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("R4CertException".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

struct Harness {
    daemon: Arc<Daemon>,
    addr: SocketAddr,
    _state: TempDir,
}

struct TempDir(std::path::PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn start() -> Harness {
    let dir = std::env::temp_dir().join(format!(
        "scd-test-{}",
        smartconnect::invite::gen_key().replace(['/', '+', '='], "")
    ));
    let identity = Identity::load_or_create(&dir, Some("test-pc".into())).unwrap();
    let acceptor = server::tls_acceptor(&identity).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let daemon = Arc::new(Daemon::new(
        identity,
        PairingStore::load(&dir).unwrap(),
        addr.port(),
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        Duration::from_secs(60),
        QrOutput {
            invert: true,
            svg_path: dir.join("qr.svg"),
        },
        dir.join("sessions"),
        Some("00:11:22:33:44:55".to_string()),
    ));
    tokio::spawn(server::serve(daemon.clone(), listener, acceptor));
    Harness {
        daemon,
        addr,
        _state: TempDir(dir),
    }
}

/// Issues an invite and returns the QR's `wsData`, parsed the way the phone does.
fn scan_qr(h: &Harness) -> Value {
    let inv = Invite::new(
        h.addr.port(),
        vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        Duration::from_secs(60),
    );
    let payload = inv.qr_payload(&h.daemon.identity, h.daemon.ble_address.as_deref());
    *h.daemon.invite.lock().unwrap() = Some(inv);
    assert!(payload.starts_with(QR_PREFIX));
    let scanned: Value = serde_json::from_str(&payload.replace(QR_PREFIX, "")).unwrap();
    assert_eq!(scanned["version"], "3.0.0");
    scanned["wsData"].clone()
}

type PhoneWs = tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;

async fn phone_connect(ws_data: &Value) -> PhoneWs {
    phone_connect_path(ws_data, "/readyfor/").await
}

async fn phone_connect_path(ws_data: &Value, path: &str) -> PhoneWs {
    let fp = B64.decode(ws_data["fp"].as_str().unwrap()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = FingerprintPin {
        expected_hex: hex::encode(fp),
        provider: provider.clone(),
    };
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();

    let ip = ws_data["policies"][0]["wifi"][0].as_str().unwrap();
    let port = ws_data["wsPort"].as_u64().unwrap();
    let tcp = TcpStream::connect(format!("{ip}:{port}")).await.unwrap();
    let tls = TlsConnector::from(Arc::new(config))
        .connect(ServerName::try_from(ip.to_owned()).unwrap(), tcp)
        .await
        .unwrap();
    let url = format!("wss://{ip}:{port}{path}");
    let (ws, _) = tokio_tungstenite::client_async(url, tls).await.unwrap();
    ws
}

fn phone_hello(token: &str) -> String {
    json!({
        "ver": "3.0.0",
        "deviceId": "phone-nonce-id",
        "srcServiceId": 0,
        "serviceId": 0,
        "senderId": 1,
        "msgId": 1,
        "data": {
            "ver": 3,
            "cmd": 0,
            "token": token,
            "paired": 0,
            "device": { "deviceName": "test phone", "realDeviceId": "phone-real-id", "verCode": 9030130 },
            "serviceVersions": { "0": 3, "9": 4 },
        },
    })
    .to_string()
}

#[tokio::test]
async fn qr_pairing_handshake_is_verified() {
    let h = start().await;
    let ws_data = scan_qr(&h);
    assert_eq!(ws_data["serviceId"], 19);
    let mut ws = phone_connect(&ws_data).await;

    ws.send(Message::text(phone_hello(
        ws_data["token"].as_str().unwrap(),
    )))
    .await
    .unwrap();
    let reply = match ws.next().await.unwrap().unwrap() {
        Message::Text(t) => serde_json::from_str::<Value>(&t).unwrap(),
        other => panic!("expected text reply, got {other:?}"),
    };

    // The phone's acceptance test: serviceId 0, cmd 0, token "OK", realDeviceId present.
    assert_eq!(reply["serviceId"], 0);
    assert_eq!(reply["data"]["cmd"], 0);
    assert_eq!(reply["data"]["ver"], 3);
    assert_eq!(reply["data"]["token"], "OK");
    assert_eq!(
        reply["data"]["device"]["realDeviceId"],
        h.daemon.identity.real_device_id.as_str()
    );
    let key = B64
        .decode(reply["data"]["device"]["key"].as_str().unwrap())
        .unwrap();
    assert_eq!(key.len(), 32);

    // The phone only brings up services the PC advertises; clipboard (9) must be listed.
    assert_eq!(reply["data"]["serviceVersions"]["9"], 4);
    assert_eq!(reply["data"]["device"]["serviceVersions"]["9"], 4);
    assert_eq!(reply["data"]["device"]["caps"]["9"], 3);
    assert_eq!(reply["data"]["device"]["sn"].as_str().unwrap().len(), 36);
}

async fn next_json(ws: &mut PhoneWs) -> Value {
    match ws.next().await.unwrap().unwrap() {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

#[tokio::test]
async fn phone_key_exchange_gets_our_key() {
    let h = start().await;
    let ws_data = scan_qr(&h);
    let mut ws = phone_connect(&ws_data).await;
    ws.send(Message::text(phone_hello(
        ws_data["token"].as_str().unwrap(),
    )))
    .await
    .unwrap();
    let reply = next_json(&mut ws).await;
    let our_key = reply["data"]["device"]["key"].as_str().unwrap().to_owned();
    assert_eq!(next_json(&mut ws).await["serviceId"], 25); // tile cap map follows the verify reply

    // The phone's sendSecretKey, shaped like the real phone's frame right after the verify reply.
    let phone_key = smartconnect::invite::gen_key();
    let exchange = json!({
        "ver": "3.0.0", "deviceId": "phone-nonce-id", "srcServiceId": 0, "serviceId": 0, "senderId": 1, "msgId": 2,
        "data": { "ver": 3, "cmd": 1, "paired": 1,
                  "device": { "deviceId": "phone-real-id", "realDeviceId": "phone-real-id", "key": phone_key } },
    });
    ws.send(Message::text(exchange.to_string())).await.unwrap();

    // EXCHANGE_KEY: our cmd 1 carrying the same per-pair key, which the phone stores to commit the pair.
    let answer = next_json(&mut ws).await;
    assert_eq!(answer["serviceId"], 0);
    assert_eq!(answer["data"]["cmd"], 1);
    assert_eq!(answer["data"]["device"]["key"], our_key.as_str());
    // The phone files the pair (and its home tile) under device.deviceId and shows the tile online
    // only if a live connection has that id as its realDeviceId — so both must be the real id.
    assert_eq!(
        answer["data"]["device"]["deviceId"],
        h.daemon.identity.real_device_id.as_str()
    );
    assert_eq!(
        answer["data"]["device"]["realDeviceId"],
        h.daemon.identity.real_device_id.as_str()
    );
    assert!(!answer["data"]["device"]["sn"].as_str().unwrap().is_empty());
    let stored = h
        .daemon
        .pairing
        .lock()
        .unwrap()
        .get("phone-real-id")
        .unwrap()
        .phone_key
        .clone();
    assert_eq!(stored.as_deref(), Some(phone_key.as_str()));
}

#[tokio::test]
async fn clipboard_nego_is_confirmed() {
    let h = start().await;
    let ws_data = scan_qr(&h);
    let mut ws = phone_connect(&ws_data).await;
    ws.send(Message::text(phone_hello(
        ws_data["token"].as_str().unwrap(),
    )))
    .await
    .unwrap();
    assert!(matches!(ws.next().await, Some(Ok(Message::Text(_)))));
    // Right after the verify reply: svc 25 `cap` map, which gates the phone's feature tiles.
    let status = match ws.next().await.unwrap().unwrap() {
        Message::Text(t) => serde_json::from_str::<Value>(&t).unwrap(),
        other => panic!("expected the svc 25 status, got {other:?}"),
    };
    assert_eq!(status["serviceId"], 25);
    assert!(status["data"]["cap"].is_object());

    // Exactly what the phone sends once the PC advertises service 9.
    let nego = json!({
        "data": { "data": { "version": 1 }, "msgId": 1 },
        "deviceId": "phone-nonce-id", "msgId": 972, "senderId": 1, "serviceId": 9, "srcServiceId": 9,
    });
    ws.send(Message::text(nego.to_string())).await.unwrap();
    let confirm = match ws.next().await.unwrap().unwrap() {
        Message::Text(t) => serde_json::from_str::<Value>(&t).unwrap(),
        other => panic!("expected CONFIRM, got {other:?}"),
    };
    assert_eq!(confirm["serviceId"], 9);
    assert_eq!(confirm["data"]["msgId"], 2);
    assert_eq!(confirm["data"]["data"]["version"], 1);
}

#[tokio::test]
async fn wrong_token_is_dropped() {
    let h = start().await;
    let ws_data = scan_qr(&h);
    let mut ws = phone_connect(&ws_data).await;
    ws.send(Message::text(phone_hello("not-the-token")))
        .await
        .unwrap();
    match ws.next().await {
        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => {}
        Some(Ok(other)) => panic!("expected the connection to drop, got {other:?}"),
    }
}

#[tokio::test]
async fn token_reusable_and_reconnect_without_token() {
    let h = start().await;
    let ws_data = scan_qr(&h);
    let token = ws_data["token"].as_str().unwrap();

    // First pair with the invite token.
    let mut first = phone_connect(&ws_data).await;
    first.send(Message::text(phone_hello(token))).await.unwrap();
    assert!(matches!(first.next().await, Some(Ok(Message::Text(_)))));

    // Reconnect of the now-paired device (same realDeviceId) with NO valid token — still accepted.
    let mut second = phone_connect(&ws_data).await;
    second
        .send(Message::text(phone_hello("not-the-token")))
        .await
        .unwrap();
    assert!(matches!(second.next().await, Some(Ok(Message::Text(_)))));
}

#[tokio::test]
async fn connect_button_trigger_gets_an_invite() {
    use smartconnect::discovery;
    let h = start().await;
    let ws_data = scan_qr(&h);
    let mut ws = phone_connect(&ws_data).await;
    ws.send(Message::text(phone_hello(
        ws_data["token"].as_str().unwrap(),
    )))
    .await
    .unwrap();
    let key = next_json(&mut ws).await["data"]["device"]["key"]
        .as_str()
        .unwrap()
        .to_owned();

    // The phone (Connect tapped) dials our /readyfor/mdns with a cmd:0 trigger sealed under our key,
    // prefixed with its own salted nonce id.
    let mut mdns = phone_connect_path(&ws_data, discovery::MDNS_PATH).await;
    let phone_nonce = B64
        .decode(smartconnect::identity::nonce_id("phone-real-id"))
        .unwrap();
    let trigger =
        discovery::seal(&key, &phone_nonce, br#"{"ver":1,"cmd":0,"serviceId":19}"#).unwrap();
    mdns.send(Message::binary(trigger)).await.unwrap();

    let frame = match mdns.next().await.unwrap().unwrap() {
        Message::Binary(b) => b,
        other => panic!("expected a binary invite, got {other:?}"),
    };
    let (prefix, kind, pt) = discovery::open(&key, &frame).unwrap();
    assert_eq!(kind, 1);
    assert!(smartconnect::identity::is_nonce_of(
        &prefix,
        &h.daemon.identity.real_device_id
    ));
    let invite: Value = serde_json::from_slice(&pt).unwrap();
    assert_eq!(invite["cmd"], 1);
    assert_eq!(invite["wsPort"], h.addr.port());
    assert_eq!(invite["fp"], ws_data["fp"]);
    // The token must get the phone back in on the control socket.
    let mut back = phone_connect(&ws_data).await;
    back.send(Message::text(phone_hello(
        invite["token"].as_str().unwrap(),
    )))
    .await
    .unwrap();
    assert_eq!(next_json(&mut back).await["data"]["token"], "OK");
}
