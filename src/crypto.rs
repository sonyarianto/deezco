use aes::Aes128;
use aes::cipher::{Array, BlockCipherEncrypt, KeyInit};
use blowfish::Blowfish;
use cbc::cipher::{BlockModeDecrypt, KeyIvInit};
use md5::{Digest, Md5};

type BlowfishCbcDec = cbc::Decryptor<Blowfish>;

pub fn md5_hex(data: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

pub fn aes_ecb_encrypt(key: &[u8], data: &[u8]) -> String {
    let cipher = Aes128::new_from_slice(key).expect("Invalid AES key length");
    let mut result = Vec::new();

    for chunk in data.chunks(16) {
        let block: [u8; 16] = chunk.try_into().expect("chunk must be 16 bytes");
        let mut block = Array::from(block);
        cipher.encrypt_block(&mut block);
        result.extend_from_slice(&block);
    }

    hex::encode(result)
}

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

pub fn decrypt_chunk(chunk: &[u8], blowfish_key: &[u8]) -> Vec<u8> {
    let iv: [u8; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
    let mut buf = chunk.to_vec();
    let mut decryptor =
        BlowfishCbcDec::new_from_slices(blowfish_key, &iv).expect("Invalid blowfish key/iv length");

    for block_data in buf.chunks_exact_mut(8) {
        let block: [u8; 8] = block_data.try_into().expect("block must be 8 bytes");
        let mut block = Array::from(block);
        decryptor.decrypt_block(&mut block);
        block_data.copy_from_slice(&block);
    }

    buf
}

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
    use cbc::cipher::BlockModeEncrypt;

    fn blowfish_cbc_encrypt(data: &[u8], key: &[u8]) -> Vec<u8> {
        let mut buf = data.to_vec();
        let mut encryptor =
            cbc::Encryptor::<Blowfish>::new_from_slices(key, &[0, 1, 2, 3, 4, 5, 6, 7]).unwrap();
        for block_data in buf.chunks_exact_mut(8) {
            let block: [u8; 8] = block_data.try_into().expect("block must be 8 bytes");
            let mut block = Array::from(block);
            encryptor.encrypt_block(&mut block);
            block_data.copy_from_slice(&block);
        }
        buf
    }

    #[test]
    fn md5_hex_matches_standard_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn aes_ecb_encrypt_matches_fips_vector() {
        assert_eq!(
            aes_ecb_encrypt(&[0u8; 16], &[0u8; 16]),
            "66e94bd4ef8a2c3b884cfa59ca342b2e"
        );
    }

    #[test]
    fn aes_ecb_encrypt_processes_blocks_independently() {
        let one = aes_ecb_encrypt(&[0u8; 16], &[0u8; 16]);
        let two = aes_ecb_encrypt(&[0u8; 16], &[0u8; 32]);
        assert_eq!(two, format!("{one}{one}"));
    }

    #[test]
    fn generate_blowfish_key_matches_known_answer() {
        let expected = [
            0x6c, 0x32, 0x63, 0x6d, 0x67, 0x36, 0x70, 0x64, 0x63, 0x7d, 0x71, 0x6c, 0x3d, 0x3b,
            0x63, 0x63,
        ];
        assert_eq!(generate_blowfish_key("123"), expected.to_vec());
    }

    #[test]
    fn generate_blowfish_key_is_deterministic_and_id_dependent() {
        let first = generate_blowfish_key("123");
        let second = generate_blowfish_key("123");
        let other = generate_blowfish_key("124");

        assert_eq!(first.len(), 16);
        assert_eq!(first, second);
        assert_ne!(first, other);
    }

    #[test]
    fn decrypt_chunk_roundtrips_blowfish_cbc() {
        let key = b"0123456789abcdef";
        let plaintext: Vec<u8> = (0..64u8).collect();

        let ciphertext = blowfish_cbc_encrypt(&plaintext, key);

        assert_eq!(decrypt_chunk(&ciphertext, key), plaintext);
    }

    #[test]
    fn decrypt_chunk_leaves_partial_trailing_block_untouched() {
        let key = b"0123456789abcdef";
        let data: Vec<u8> = (0..20u8).collect();

        let decrypted = decrypt_chunk(&data, key);

        assert_eq!(decrypted.len(), data.len());
        assert_eq!(&decrypted[16..], &data[16..]);
    }

    #[test]
    fn decrypt_stream_reverses_the_stream_format() {
        let key = b"0123456789abcdef";
        let plaintext: Vec<u8> = (0..=255u8).cycle().take(2048 * 3 + 100).collect();

        let mut encrypted = Vec::new();
        let mut offset = 0;
        while offset < plaintext.len() {
            let end = (offset + 2048 * 3).min(plaintext.len());
            let chunk = &plaintext[offset..end];
            if chunk.len() >= 2048 {
                encrypted.extend_from_slice(&blowfish_cbc_encrypt(&chunk[..2048], key));
                encrypted.extend_from_slice(&chunk[2048..]);
            } else {
                encrypted.extend_from_slice(chunk);
            }
            offset = end;
        }

        assert_eq!(decrypt_stream(&encrypted, key), plaintext);
    }

    #[test]
    fn decrypt_stream_passes_through_short_input() {
        let key = b"0123456789abcdef";
        let data: Vec<u8> = (0..100u8).collect();

        assert_eq!(decrypt_stream(&data, key), data);
    }
}
