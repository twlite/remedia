use remedia_core::EncodedOpusPacket;

use crate::crypto::{EncryptionMode, TransportCipher};
use crate::dave::DaveProtocol;
use crate::error::VoiceError;

pub(crate) const RTP_HEADER_BYTES: usize = 12;
pub(crate) const MAX_UDP_PACKET_BYTES: usize = 1_200;
const RTP_AUDIO_PAYLOAD_TYPE: u8 = 120;

pub(crate) fn serialize_rtp_header(sequence: u16, timestamp: u32, ssrc: u32) -> [u8; 12] {
    let mut header = [0; RTP_HEADER_BYTES];
    header[0] = 0x80; // RTP version 2, no CSRC or extension.
    header[1] = RTP_AUDIO_PAYLOAD_TYPE;
    header[2..4].copy_from_slice(&sequence.to_be_bytes());
    header[4..8].copy_from_slice(&timestamp.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());
    header
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RtpClock {
    pub sequence: u16,
    pub timestamp: u32,
}

impl RtpClock {
    pub(crate) fn advance_sent(&mut self, samples_per_channel: u32) {
        self.sequence = self.sequence.wrapping_add(1);
        self.timestamp = self.timestamp.wrapping_add(samples_per_channel);
    }

    pub(crate) fn advance_dropped(&mut self, samples_per_channel: u32) {
        self.timestamp = self.timestamp.wrapping_add(samples_per_channel);
    }
}

pub(crate) struct RtpSender {
    ssrc: u32,
    clock: RtpClock,
    nonce: u32,
    cipher: TransportCipher,
}

impl RtpSender {
    pub(crate) fn new(
        ssrc: u32,
        sequence: u16,
        timestamp: u32,
        nonce: u32,
        mode: EncryptionMode,
        secret_key: &[u8],
    ) -> Result<Self, VoiceError> {
        Ok(Self {
            ssrc,
            clock: RtpClock {
                sequence,
                timestamp,
            },
            nonce,
            cipher: TransportCipher::new(mode, secret_key)?,
        })
    }

    pub(crate) fn encode_packet(
        &mut self,
        packet: &EncodedOpusPacket,
        dave: &mut DaveProtocol,
    ) -> Result<Vec<u8>, VoiceError> {
        validate_opus_packet(packet)?;
        if !dave.is_ready() {
            return Err(VoiceError::DaveNotReady);
        }
        let dave_payload = dave.encrypt_opus(&packet.bytes)?;
        let header = serialize_rtp_header(self.clock.sequence, self.clock.timestamp, self.ssrc);
        let encrypted_payload =
            self.cipher
                .encrypt_payload(&header, self.nonce, dave_payload.as_ref())?;
        let packet_size = header.len() + encrypted_payload.len();
        if packet_size > MAX_UDP_PACKET_BYTES {
            return Err(VoiceError::PacketTooLarge {
                actual: packet_size,
                maximum: MAX_UDP_PACKET_BYTES,
            });
        }

        let mut bytes = Vec::with_capacity(packet_size);
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&encrypted_payload);
        self.clock.advance_sent(packet.samples_per_channel);
        self.nonce = self.nonce.wrapping_add(1);
        Ok(bytes)
    }

    pub(crate) fn skip_packet_duration(&mut self, samples_per_channel: u32) {
        self.clock.advance_dropped(samples_per_channel);
    }
}

pub(crate) fn validate_opus_packet(packet: &EncodedOpusPacket) -> Result<(), VoiceError> {
    let legal = matches!(
        packet.samples_per_channel,
        120 | 240 | 480 | 960 | 1_920 | 2_880 | 5_760
    );
    if !legal || packet.media_samples_per_channel > packet.samples_per_channel {
        return Err(VoiceError::InvalidOpusDuration(packet.samples_per_channel));
    }
    if packet.bytes.is_empty() {
        return Err(VoiceError::Protocol(
            "Opus packet contained no bytes".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtp_header_is_serialized_in_network_byte_order() {
        assert_eq!(
            serialize_rtp_header(0x1234, 0x0102_0304, 0xA0B0_C0D0),
            [
                0x80, 120, 0x12, 0x34, 0x01, 0x02, 0x03, 0x04, 0xA0, 0xB0, 0xC0, 0xD0
            ]
        );
    }

    #[test]
    fn rtp_sequence_and_timestamp_wrap_and_dropped_frames_keep_time() {
        let mut clock = RtpClock {
            sequence: u16::MAX,
            timestamp: u32::MAX - 959,
        };
        clock.advance_sent(960);
        assert_eq!(clock.sequence, 0);
        assert_eq!(clock.timestamp, 0);
        clock.advance_dropped(960);
        assert_eq!(clock.sequence, 0);
        assert_eq!(clock.timestamp, 960);
    }

    #[test]
    fn rejects_illegal_opus_durations_before_packetization() {
        let mut packet = EncodedOpusPacket {
            bytes: vec![1],
            samples_per_channel: 400,
            media_samples_per_channel: 400,
            start_sample: 0,
        };
        assert!(matches!(
            validate_opus_packet(&packet),
            Err(VoiceError::InvalidOpusDuration(400))
        ));
        packet.samples_per_channel = 960;
        packet.media_samples_per_channel = 961;
        assert!(validate_opus_packet(&packet).is_err());
    }
}
