use remedia_core::EncodedOpusPacket;

use crate::crypto::{AUTH_TAG_BYTES, EncryptionMode, TRANSPORT_NONCE_BYTES, TransportCipher};
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
    /// Reused DAVE and RTP datagram storage. Capacity is bounded by the largest accepted
    /// datagram, so steady-state packetization performs no heap allocation.
    datagram: Vec<u8>,
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
            datagram: Vec::with_capacity(MAX_UDP_PACKET_BYTES),
        })
    }

    /// Packetize and encrypt `packet`, returning the datagram to transmit.
    ///
    /// The returned slice borrows this sender's reusable buffer and is valid until the next call.
    /// Neither the RTP clock nor the nonce advances unless the whole datagram was built.
    pub(crate) fn encode_packet(
        &mut self,
        packet: &EncodedOpusPacket,
        dave: &mut DaveProtocol,
    ) -> Result<&[u8], VoiceError> {
        validate_opus_packet(packet)?;
        if !dave.is_ready() {
            return Err(VoiceError::DaveNotReady);
        }
        self.datagram.clear();
        if let Err(error) = dave.encrypt_opus_into(&packet.bytes, &mut self.datagram) {
            // DAVE resizes output before encrypting. Failed encryption must not retain
            // oversized scratch storage on the connection either.
            if self.datagram.capacity() > MAX_UDP_PACKET_BYTES {
                self.datagram = Vec::with_capacity(MAX_UDP_PACKET_BYTES);
            }
            return Err(error);
        }

        let dave_payload_bytes = self.datagram.len();
        let header = serialize_rtp_header(self.clock.sequence, self.clock.timestamp, self.ssrc);
        let packet_size =
            RTP_HEADER_BYTES + dave_payload_bytes + AUTH_TAG_BYTES + TRANSPORT_NONCE_BYTES;
        if packet_size > MAX_UDP_PACKET_BYTES {
            // A caller-supplied oversized frame can make DAVE grow its output Vec before the
            // transport limit is known. Drop that oversized storage rather than retaining it on
            // this long-lived connection.
            self.datagram = Vec::with_capacity(MAX_UDP_PACKET_BYTES);
            return Err(VoiceError::PacketTooLarge {
                actual: packet_size,
                maximum: MAX_UDP_PACKET_BYTES,
            });
        }

        self.datagram
            .resize(RTP_HEADER_BYTES + dave_payload_bytes, 0);
        self.datagram
            .copy_within(0..dave_payload_bytes, RTP_HEADER_BYTES);
        self.datagram[..RTP_HEADER_BYTES].copy_from_slice(&header);
        self.cipher
            .seal_in_place(&header, self.nonce, &mut self.datagram, RTP_HEADER_BYTES)?;
        debug_assert_eq!(self.datagram.len(), packet_size);
        self.clock.advance_sent(packet.samples_per_channel);
        self.nonce = self.nonce.wrapping_add(1);
        Ok(&self.datagram)
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

    #[test]
    fn rejected_oversized_dave_output_does_not_remain_in_the_reused_datagram() {
        let (_, mut dave, _) = crate::dave::tests::established_group();
        let mut sender = RtpSender::new(1, 0, 0, 0, EncryptionMode::Aes256Gcm, &[9; 32]).unwrap();
        let packet = EncodedOpusPacket {
            bytes: vec![0x55; 2_000],
            samples_per_channel: 960,
            media_samples_per_channel: 960,
            start_sample: 0,
        };

        assert!(matches!(
            sender.encode_packet(&packet, &mut dave),
            Err(VoiceError::PacketTooLarge { .. })
        ));
        assert!(sender.datagram.capacity() <= MAX_UDP_PACKET_BYTES);

        let valid_packet = EncodedOpusPacket {
            bytes: vec![0x55; 90],
            samples_per_channel: 960,
            media_samples_per_channel: 960,
            start_sample: 0,
        };
        let datagram = sender.encode_packet(&valid_packet, &mut dave).unwrap();
        assert_eq!(datagram[2..4], 0_u16.to_be_bytes());
        assert_eq!(datagram[4..8], 0_u32.to_be_bytes());
        assert_eq!(&datagram[datagram.len() - TRANSPORT_NONCE_BYTES..], &[0; 4]);
    }
}
