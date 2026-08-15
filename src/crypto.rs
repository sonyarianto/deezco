use aes::Aes128;
use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use blowfish::Blowfish;
use cbc::cipher::{BlockDecryptMut, KeyIvInit};
use md5::{Digest, Md5};

type BlowfishCbcDec = cbc::Decryptor<Blowfish>;

/// MD5 hash returning hex string
pub fn md5_hex(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// AES-128-ECB encrypt (no padding) - returns hex string
pub fn aes_ecb_encrypt(key: &[u8], data: &[u8]) -> String {
    let cipher = Aes128::new(key.into());
    let mut result = Vec::new();

    for chunk in data.chunks(16) {
        let mut block = aes::cipher::generic_array::GenericArray::clone_from_slice(chunk);
        cipher.encrypt_block(&mut block);
        result.extend_from_slice(&block);
    }

    hex::encode(result)
}

/// Generate the Blowfish key for track decryption
pub fn generate_blowfish_key(track_id: &str) -> Vec<u8> {
    const SECRET: &[u8] = b"g4el58wc0zvf9na1";
    let id_md5 = md5_hex(track_id.as_bytes());
    let id_md5_bytes = id_md5.as_bytes();

    let mut bf_key = Vec::with_capacity(16);
    for i in 0..16 {
        bf_key.push(id_md5_bytes[i] ^ id_md5_bytes[i + 16] ^ SECRET[i]);
    }
    bf_key
}

/// Decrypt a 2048-byte chunk with Blowfish CBC
pub fn decrypt_chunk(chunk: &[u8], blowfish_key: &[u8]) -> Vec<u8> {
    let iv: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
    let mut buf = chunk.to_vec();
    let mut decryptor =
        BlowfishCbcDec::new_from_slices(blowfish_key, &iv).expect("Invalid blowfish key/iv length");

    // Blowfish block size is 8 bytes; decrypt each block in place (CBC).
    // Partial trailing blocks are left untouched, matching the previous behavior.
    for block in buf.chunks_exact_mut(8) {
        decryptor.decrypt_block_mut(GenericArray::from_mut_slice(block));
    }

    buf
}

/// Generate the encrypted stream URL path
pub fn generate_stream_path(sng_id: &str, md5: &str, media_version: &str, format: u32) -> String {
    let url_part_raw = format!(
        "{}\u{00a4}{}\u{00a4}{}\u{00a4}{}",
        md5, format, sng_id, media_version
    );
    let md5val = md5_hex(url_part_raw.as_bytes());
    let mut step2 = format!("{}\u{00a4}{}\u{00a4}", md5val, url_part_raw);
    let pad_len = 16 - (step2.len() % 16);
    if pad_len < 16 {
        step2.push_str(&".".repeat(pad_len));
    }

    aes_ecb_encrypt(b"jo6aey6haid2Teih", step2.as_bytes())
}

/// Generate the full crypted stream URL
pub fn generate_crypted_stream_url(
    sng_id: &str,
    md5: &str,
    media_version: &str,
    format: u32,
) -> String {
    let url_part = generate_stream_path(sng_id, md5, media_version, format);
    let first_char = md5.chars().next().unwrap_or('0');
    format!(
        "https://e-cdns-proxy-{}.dzcdn.net/mobile/1/{}",
        first_char, url_part
    )
}

/// Decrypt a full encrypted stream, processing 2048*3-byte blocks
pub fn decrypt_stream(encrypted: &[u8], blowfish_key: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(encrypted.len());
    let mut offset = 0;
    let chunk_size = 2048 * 3;

    while offset < encrypted.len() {
        let remaining = encrypted.len() - offset;
        let current_chunk_size = remaining.min(chunk_size);
        let chunk = &encrypted[offset..offset + current_chunk_size];

        if chunk.len() >= 2048 {
            let decrypted = decrypt_chunk(&chunk[..2048], blowfish_key);
            output.extend_from_slice(&decrypted);
            output.extend_from_slice(&chunk[2048..]);
        } else {
            output.extend_from_slice(chunk);
        }

        offset += current_chunk_size;
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use cbc::cipher::BlockEncryptMut;

    #[test]
    fn decrypt_chunk_roundtrips_blowfish_cbc() {
        let key = b"0123456789abcdef";
        let plaintext: Vec<u8> = (0..64u8).collect();

        // Encrypt with Blowfish-CBC using the same IV as the decoder
        let mut ciphertext = plaintext.clone();
        let mut encryptor =
            cbc::Encryptor::<Blowfish>::new_from_slices(key, &[0, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        for block in ciphertext.chunks_exact_mut(8) {
            encryptor.encrypt_block_mut(GenericArray::from_mut_slice(block));
        }

        assert_eq!(decrypt_chunk(&ciphertext, key), plaintext);
    }

    #[test]
    fn decrypt_chunk_leaves_partial_trailing_block_untouched() {
        let key = b"0123456789abcdef";
        let data: Vec<u8> = (0..20u8).collect(); // 16 full bytes + 4 trailing

        let decrypted = decrypt_chunk(&data, key);

        assert_eq!(decrypted.len(), data.len());
        assert_eq!(&decrypted[16..], &data[16..]);
    }
}
