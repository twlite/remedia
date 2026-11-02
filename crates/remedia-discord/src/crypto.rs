use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{AeadInOut, KeyInit, inout::InOutBuf};
use chacha20poly1305::XChaCha20Poly1305;

use crate::error::VoiceError;

pub(crate) const TRANSPORT_NONCE_BYTES: usize = 4;
#[cfg(test)]
const AUTH_TAG_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EncryptionMode {
    Aes256Gcm,
    XChaCha20Poly1305,
}

impl EncryptionMode {
    pub(crate) fn choose(offered: &[String]) -> Result<Self, VoiceError> {
        if offered.iter().any(|mode| mode == "aead_aes256_gcm_rtpsize") {
            Ok(Self::Aes256Gcm)
        } else if offered
            .iter()
            .any(|mode| mode == "aead_xchacha20_poly1305_rtpsize")
        {
            Ok(Self::XChaCha20Poly1305)
        } else {
            Err(VoiceError::EncryptionUnavailable)
        }
    }

    pub(crate) const fn wire_name(self) -> &'static str {
        match self {
            Self::Aes256Gcm => "aead_aes256_gcm_rtpsize",
            Self::XChaCha20Poly1305 => "aead_xchacha20_poly1305_rtpsize",
        }
    }
}

#[derive(Clone)]
pub(crate) enum TransportCipher {
    Aes256Gcm(Box<Aes256Gcm>),
    XChaCha20Poly1305(XChaCha20Poly1305),
}

impl TransportCipher {
    pub(crate) fn new(mode: EncryptionMode, secret_key: &[u8]) -> Result<Self, VoiceError> {
        if secret_key.len() != 32 {
            return Err(VoiceError::InvalidSessionKey);
        }
        match mode {
            EncryptionMode::Aes256Gcm => Aes256Gcm::new_from_slice(secret_key)
                .map(Box::new)
                .map(Self::Aes256Gcm)
                .map_err(|error| VoiceError::Encryption(error.to_string())),
            EncryptionMode::XChaCha20Poly1305 => XChaCha20Poly1305::new_from_slice(secret_key)
                .map(Self::XChaCha20Poly1305)
                .map_err(|error| VoiceError::Encryption(error.to_string())),
        }
    }

    /// Encrypt a payload using RTP header bytes as authenticated data.
    /// The packet suffix follows Discord's RTP-size format: tag then the
    /// network-order 32-bit nonce counter.
    pub(crate) fn encrypt_payload(
        &self,
        rtp_header: &[u8; 12],
        nonce_counter: u32,
        payload: &[u8],
    ) -> Result<Vec<u8>, VoiceError> {
        let mut ciphertext = payload.to_vec();
        let nonce_suffix = nonce_counter.to_be_bytes();
        let tag = match self {
            Self::Aes256Gcm(cipher) => {
                let mut nonce = aes_gcm::Nonce::default();
                nonce[..TRANSPORT_NONCE_BYTES].copy_from_slice(&nonce_suffix);
                cipher
                    .encrypt_inout_detached(&nonce, rtp_header, InOutBuf::from(&mut ciphertext[..]))
                    .map_err(|error| VoiceError::Encryption(error.to_string()))?
                    .to_vec()
            }
            Self::XChaCha20Poly1305(cipher) => {
                let mut nonce = chacha20poly1305::XNonce::default();
                nonce[..TRANSPORT_NONCE_BYTES].copy_from_slice(&nonce_suffix);
                cipher
                    .encrypt_inout_detached(&nonce, rtp_header, InOutBuf::from(&mut ciphertext[..]))
                    .map_err(|error| VoiceError::Encryption(error.to_string()))?
                    .to_vec()
            }
        };
        ciphertext.extend_from_slice(&tag);
        ciphertext.extend_from_slice(&nonce_suffix);
        Ok(ciphertext)
    }

    #[cfg(test)]
    fn decrypt_payload(
        &self,
        rtp_header: &[u8; 12],
        encrypted: &[u8],
    ) -> Result<Vec<u8>, VoiceError> {
        if encrypted.len() < AUTH_TAG_BYTES + TRANSPORT_NONCE_BYTES {
            return Err(VoiceError::Encryption(
                "encrypted payload was truncated".into(),
            ));
        }
        let split = encrypted.len() - AUTH_TAG_BYTES - TRANSPORT_NONCE_BYTES;
        let tag_start = split;
        let nonce_start = encrypted.len() - TRANSPORT_NONCE_BYTES;
        let nonce_bytes = &encrypted[nonce_start..];
        let mut plaintext = encrypted[..split].to_vec();
        match self {
            Self::Aes256Gcm(cipher) => {
                let mut nonce = aes_gcm::Nonce::default();
                nonce[..TRANSPORT_NONCE_BYTES].copy_from_slice(nonce_bytes);
                let tag: aes_gcm::Tag = encrypted[tag_start..nonce_start]
                    .try_into()
                    .map_err(|_| VoiceError::Encryption("invalid AES-GCM tag size".into()))?;
                cipher
                    .decrypt_inout_detached(
                        &nonce,
                        rtp_header,
                        InOutBuf::from(&mut plaintext[..]),
                        &tag,
                    )
                    .map_err(|error| VoiceError::Encryption(error.to_string()))?;
            }
            Self::XChaCha20Poly1305(cipher) => {
                let mut nonce = chacha20poly1305::XNonce::default();
                nonce[..TRANSPORT_NONCE_BYTES].copy_from_slice(nonce_bytes);
                let tag: chacha20poly1305::Tag = encrypted[tag_start..nonce_start]
                    .try_into()
                    .map_err(|_| VoiceError::Encryption("invalid XChaCha tag size".into()))?;
                cipher
                    .decrypt_inout_detached(
                        &nonce,
                        rtp_header,
                        InOutBuf::from(&mut plaintext[..]),
                        &tag,
                    )
                    .map_err(|error| VoiceError::Encryption(error.to_string()))?;
            }
        }
        Ok(plaintext)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_negotiated_transport_modes_round_trip_with_rtp_associated_data() {
        let key = [0x5a; 32];
        let header = [0x80, 0x78, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let payload = b"encoded opus packet";

        for mode in [EncryptionMode::Aes256Gcm, EncryptionMode::XChaCha20Poly1305] {
            let cipher = TransportCipher::new(mode, &key).unwrap();
            let encrypted = cipher.encrypt_payload(&header, u32::MAX, payload).unwrap();
            assert_eq!(
                cipher.decrypt_payload(&header, &encrypted).unwrap(),
                payload
            );
            let mut modified_header = header;
            modified_header[2] ^= 1;
            assert!(
                cipher
                    .decrypt_payload(&modified_header, &encrypted)
                    .is_err()
            );
        }
    }

    #[test]
    fn prefers_aes_when_available_and_requires_xchacha_fallback() {
        assert_eq!(
            EncryptionMode::choose(&[
                "aead_xchacha20_poly1305_rtpsize".into(),
                "aead_aes256_gcm_rtpsize".into(),
            ])
            .unwrap(),
            EncryptionMode::Aes256Gcm
        );
        assert_eq!(
            EncryptionMode::choose(&["aead_xchacha20_poly1305_rtpsize".into()]).unwrap(),
            EncryptionMode::XChaCha20Poly1305
        );
        assert!(EncryptionMode::choose(&["xsalsa20_poly1305_lite".into()]).is_err());
    }
}
