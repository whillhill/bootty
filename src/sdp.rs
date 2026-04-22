use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use anyhow::{Context, Result};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionDescription {
    pub sdp: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub ten_kb_site_loc: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub key: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub nonce: String,
}

impl SessionDescription {
    pub fn encode(&self) -> Result<String> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(self.sdp.as_bytes())?;
        let compressed = encoder.finish()?;
        let encoded_sdp = bs58::encode(&compressed).into_string();

        let mut copy = self.clone();
        copy.sdp = encoded_sdp;

        let json = serde_json::to_vec(&copy)?;
        Ok(bs58::encode(&json).into_string())
    }

    pub fn decode(input: &str) -> Result<Self> {
        let decoded = bs58::decode(input)
            .into_vec()
            .context("base58 decode outer failed")?;
        let mut sd: SessionDescription =
            serde_json::from_slice(&decoded).context("json decode failed")?;

        let compressed = bs58::decode(&sd.sdp)
            .into_vec()
            .context("base58 decode inner sdp failed")?;
        let mut decoder = ZlibDecoder::new(&compressed[..]);
        let mut sdp = String::new();
        decoder.read_to_string(&mut sdp)?;
        sd.sdp = sdp;

        Ok(sd)
    }

    pub fn gen_keys(&mut self) -> Result<()> {
        let mut key = [0u8; 32];
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut key);
        rand::thread_rng().fill_bytes(&mut nonce);
        self.key = hex::encode(key);
        self.nonce = hex::encode(nonce);
        Ok(())
    }

    pub fn encrypt(&mut self) -> Result<()> {
        let key_bytes = hex::decode(&self.key).context("decode key hex")?;
        let nonce_bytes = hex::decode(&self.nonce).context("decode nonce hex")?;

        let cipher = Aes256Gcm::new_from_slice(&key_bytes)?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = cipher.encrypt(nonce, self.sdp.as_bytes())?;
        self.sdp = hex::encode(ciphertext);
        Ok(())
    }

    pub fn decrypt(&mut self) -> Result<()> {
        let key_bytes = hex::decode(&self.key).context("decode key hex")?;
        let nonce_bytes = hex::decode(&self.nonce).context("decode nonce hex")?;
        let ciphertext = hex::decode(&self.sdp).context("decode ciphertext hex")?;

        let cipher = Aes256Gcm::new_from_slice(&key_bytes)?;
        let nonce = Nonce::from_slice(&nonce_bytes);
        let plaintext = cipher.decrypt(nonce, ciphertext.as_ref())?;
        self.sdp = String::from_utf8(plaintext)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip() {
        let sd = SessionDescription {
            sdp: "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\n".to_string(),
            ten_kb_site_loc: "".to_string(),
            key: "".to_string(),
            nonce: "".to_string(),
        };
        let encoded = sd.encode().unwrap();
        let decoded = SessionDescription::decode(&encoded).unwrap();
        assert_eq!(sd.sdp, decoded.sdp);
    }

    #[test]
    fn test_encrypt_decrypt() {
        let mut sd = SessionDescription {
            sdp: "this is a secret sdp".to_string(),
            ten_kb_site_loc: "".to_string(),
            key: "".to_string(),
            nonce: "".to_string(),
        };
        sd.gen_keys().unwrap();
        let original = sd.sdp.clone();
        sd.encrypt().unwrap();
        assert_ne!(sd.sdp, original);
        sd.decrypt().unwrap();
        assert_eq!(sd.sdp, original);
    }

    #[test]
    fn test_roundtrip_full_fields() {
        let sd = SessionDescription {
            sdp: "v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\n".to_string(),
            ten_kb_site_loc: "abc123".to_string(),
            key: "deadbeef".to_string(),
            nonce: "cafebabe".to_string(),
        };
        let encoded = sd.encode().unwrap();
        let decoded = SessionDescription::decode(&encoded).unwrap();
        assert_eq!(sd.sdp, decoded.sdp);
        assert_eq!(sd.ten_kb_site_loc, decoded.ten_kb_site_loc);
        assert_eq!(sd.key, decoded.key);
        assert_eq!(sd.nonce, decoded.nonce);
    }
}
