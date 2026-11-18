//! ESP32 bodies (Phase 7, §8): two paths, ONE brain.
//!
//! * XiaoZhi path: the device speaks WebSocket to the hub's
//!   WS server (port 19121), after an OTA check that hands it
//!   a session token. The hub never claims E2E for this path
//!   (the firmware does not pin a certificate) and never
//!   claims the robot can hear/speak: voice complete is
//!   OUTSIDE Phase 7. Audio frames are accepted on the wire
//!   and answered with the unsupported-voice notice — they
//!   are never transcribed.
//! * v2 path: the device speaks the friends session protocol
//!   (mutual authentication by pinned identity key + ECDH
//!   ephemeral + AEAD) to the hub's friends port, exactly
//!   like a peer body. The reference client below runs on
//!   the host; the ESP-IDF skeleton lives in
//!   `firmware/mimint-esp32-v2/` (toolchain-gated).
//!
//! Both paths call [`handle_esp32_turn`] — the SAME brain
//! adapter, the same code path. The `source` field records
//! which path the turn came from; it never changes what the
//! brain does.

use crate::error::MimintError;
use crate::friends::identity::Identity;
use crate::friends::session::{handshake_initiator, Session, SessionConfig};
use crate::friends::texts;
use crate::friends::wire::MessageKind;
use crate::friends::ws::{WsConnection, WsFrame};
use serde::{Deserialize, Serialize};
use std::net::TcpStream;
use std::time::Duration;

/// Which path a turn came from. Recorded, never branching
/// the brain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Esp32Path {
    XiaoZhi,
    V2,
}

/// Input to one brain turn from an ESP32 body.
#[derive(Debug, Clone)]
pub struct Esp32Input {
    pub source: Esp32Path,
    pub device_uuid: String,
    /// Text from the device (transcribed by the device side,
    /// or typed in the fake device). Empty when the turn
    /// carries audio only.
    pub text: String,
    /// Audio bytes, when present. Never transcribed in P7.
    pub audio: Vec<u8>,
}

/// Output of one brain turn to an ESP32 body.
#[derive(Debug, Clone)]
pub struct Esp32Output {
    /// Text to speak/display on the device.
    pub text: String,
    /// Servo commands the brain asked for (validated by the
    /// caller through the allow-list before dispatch).
    pub servo_commands: Vec<crate::friends::servo::ServoCommand>,
}

/// The single brain adapter: BOTH paths call this, with the
/// same brain closure. `brain_turn` is one turn of the body
/// brain (in production: `Brain::run` on the received text).
/// Audio-only input is answered with the unsupported-voice
/// notice — Phase 7 does not transcribe audio.
pub fn handle_esp32_turn<F>(brain_turn: F, input: &Esp32Input) -> Esp32Output
where
    F: Fn(&str) -> String,
{
    if input.text.trim().is_empty() && !input.audio.is_empty() {
        return Esp32Output {
            text: texts::ESP32_LABEL_VOICE_UNSUPPORTED.to_string(),
            servo_commands: Vec::new(),
        };
    }
    let reply = brain_turn(&input.text);
    Esp32Output {
        text: reply,
        servo_commands: Vec::new(),
    }
}

/// The exact ESP32 status label (§8.5): one of four, no
/// others. Lab mode, XiaoZhi LAN, v2, or voice-unsupported.
pub fn esp32_status_label(path: Esp32Path, lab_mode: bool, paired: bool) -> &'static str {
    match (path, lab_mode, paired) {
        (_, true, _) => texts::ESP32_LABEL_LAB,
        (Esp32Path::XiaoZhi, false, _) => texts::ESP32_LABEL_XIAOZHI_LAN,
        (Esp32Path::V2, false, true) => texts::ESP32_LABEL_V2,
        (Esp32Path::V2, false, false) => texts::ESP32_LABEL_XIAOZHI_LAN,
    }
}

/// The v2 reference client: connect to the hub friends port
/// and complete the friends handshake as an initiator. The
/// hub's identity key must already be PINNED in this device's
/// registry (pairing first, then connect) — the handshake
/// checks the pin BEFORE signing, exactly like a peer body.
/// Returns the live session.
pub fn v2_client_handshake(
    stream: TcpStream,
    identity: &Identity,
    registry: &crate::friends::registry::DeviceRegistry,
    clock: &crate::friends::util::Clock,
) -> Result<Session, MimintError> {
    let config = SessionConfig::default();
    handshake_initiator(stream, identity, registry, &config, clock, None)
}

/// A hello message on the XiaoZhi WS channel (§8.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XiaoZhiHello {
    pub r#type: String,
    pub device_id: String,
    pub version: u32,
}

/// A text message on the XiaoZhi WS channel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XiaoZhiText {
    pub r#type: String,
    pub text: String,
}

/// The fake XiaoZhi device (host): OTA token in hand, it
/// opens the WS channel, says hello, and exchanges one text
/// turn. Used by the e2e tests and the G2 demo.
pub struct FakeXiaoZhiDevice {
    conn: WsConnection,
    pub uuid: String,
}

impl FakeXiaoZhiDevice {
    pub fn connect(addr: &str, uuid: &str, token: &str) -> Result<Self, MimintError> {
        let stream = TcpStream::connect(addr)
            .map_err(|err| MimintError::Io(format!("fake xiaozhi connect: {err}")))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|err| MimintError::Io(err.to_string()))?;
        let path = format!("/xiaozhi?token={token}");
        let conn = WsConnection::connect_upgrade(stream, addr, &path).map_err(MimintError::from)?;
        let mut device = Self {
            conn,
            uuid: uuid.to_string(),
        };
        let hello = XiaoZhiHello {
            r#type: "hello".to_string(),
            device_id: uuid.to_string(),
            version: 2,
        };
        device.send_json(&serde_json::to_value(&hello).unwrap())?;
        Ok(device)
    }

    pub fn send_json(&mut self, value: &serde_json::Value) -> Result<(), MimintError> {
        let text = serde_json::to_string(value)
            .map_err(|err| MimintError::Config(format!("fake xiaozhi serialize: {err}")))?;
        self.conn
            .write_frame(&WsFrame::Text(text.into_bytes()))
            .map_err(MimintError::from)
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), MimintError> {
        self.send_json(&serde_json::json!({"type": "text", "text": text}))
    }

    /// Read one text frame; ping frames are answered
    /// automatically.
    pub fn read_text(&mut self) -> Result<String, MimintError> {
        loop {
            match self.conn.read_frame().map_err(MimintError::from)? {
                WsFrame::Text(bytes) => {
                    return String::from_utf8(bytes).map_err(|_| {
                        MimintError::Config("fake xiaozhi: non-UTF8 text frame".to_string())
                    });
                }
                WsFrame::Ping(payload) => {
                    self.conn
                        .write_frame(&WsFrame::Pong(payload))
                        .map_err(MimintError::from)?;
                }
                WsFrame::Close(_) => {
                    return Err(MimintError::Config(
                        "fake xiaozhi: server closed the connection".to_string(),
                    ));
                }
                _ => {}
            }
        }
    }

    pub fn close(&mut self) {
        self.conn.close(1000, "done");
    }
}

/// The fake v2 device (host): a friends session to the hub
/// plus one text turn.
pub struct FakeV2Device {
    session: Session,
}

impl FakeV2Device {
    pub fn connect(
        addr: &str,
        identity: &Identity,
        registry: &crate::friends::registry::DeviceRegistry,
        clock: &crate::friends::util::Clock,
    ) -> Result<Self, MimintError> {
        let stream = TcpStream::connect(addr)
            .map_err(|err| MimintError::Io(format!("fake v2 connect: {err}")))?;
        let session = v2_client_handshake(stream, identity, registry, clock)?;
        Ok(Self { session })
    }

    pub fn send_text(&mut self, text: &str) -> Result<(), MimintError> {
        let payload =
            serde_json::to_vec(&serde_json::json!({"type": "text", "text": text})).unwrap();
        // Reuse the sync_push kind as a generic text carrier
        // for the fake device only — the hub's v2 handler
        // reads the JSON and answers with the brain reply.
        self.session.send(MessageKind::SyncPush, &payload)
    }

    pub fn read_text(&mut self) -> Result<String, MimintError> {
        let (kind, payload) = self.session.recv()?;
        if kind != MessageKind::SyncPush {
            return Err(MimintError::Config(format!(
                "fake v2: expected a text reply, got {}",
                kind.as_str()
            )));
        }
        let value: serde_json::Value = serde_json::from_slice(&payload)
            .map_err(|err| MimintError::Config(format!("fake v2: bad reply: {err}")))?;
        Ok(value["text"].as_str().unwrap_or("").to_string())
    }

    pub fn close(self) {
        self.session.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esp32_both_paths_use_the_same_brain_adapter() {
        // The SAME closure answers both paths: one brain.
        let brain = |text: &str| format!("echo: {text}");
        let xiaozhi = handle_esp32_turn(
            brain,
            &Esp32Input {
                source: Esp32Path::XiaoZhi,
                device_uuid: "uuid-1".to_string(),
                text: "hello".to_string(),
                audio: Vec::new(),
            },
        );
        let v2 = handle_esp32_turn(
            brain,
            &Esp32Input {
                source: Esp32Path::V2,
                device_uuid: "uuid-1".to_string(),
                text: "hello".to_string(),
                audio: Vec::new(),
            },
        );
        assert_eq!(xiaozhi.text, v2.text);
        assert_eq!(xiaozhi.text, "echo: hello");
        // Audio-only input is never transcribed.
        let audio_only = handle_esp32_turn(
            brain,
            &Esp32Input {
                source: Esp32Path::XiaoZhi,
                device_uuid: "uuid-1".to_string(),
                text: String::new(),
                audio: vec![0u8; 100],
            },
        );
        assert!(audio_only.text.contains("chưa hỗ trợ"));
    }

    #[test]
    fn esp32_status_labels_are_exactly_four() {
        assert_eq!(
            esp32_status_label(Esp32Path::XiaoZhi, true, true),
            texts::ESP32_LABEL_LAB
        );
        assert_eq!(
            esp32_status_label(Esp32Path::XiaoZhi, false, true),
            texts::ESP32_LABEL_XIAOZHI_LAN
        );
        assert_eq!(
            esp32_status_label(Esp32Path::V2, false, true),
            texts::ESP32_LABEL_V2
        );
        // An unpaired v2 device shows the limited-auth label.
        assert_eq!(
            esp32_status_label(Esp32Path::V2, false, false),
            texts::ESP32_LABEL_XIAOZHI_LAN
        );
    }
}
