use num_bigint::BigUint;
use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::{Digest, Sha256};
use srp::client::SrpClient;
use srp::groups::G_2048;
use srp::utils::compute_k;

use crate::{ProtoError, Result};

/// Apple's s2k / s2k_fo password key. This is the input to SRP, not the verifier.
///
/// `s2k` feeds PBKDF2 the raw SHA-256 of the password. `s2k_fo` feeds it the hex
/// encoding of that digest.
pub fn derive_password(password: &str, salt: &[u8], iterations: u32, protocol: &str) -> [u8; 32] {
    let digest = Sha256::digest(password.as_bytes());
    let material: Vec<u8> = if protocol == "s2k_fo" {
        hex_encode(&digest).into_bytes()
    } else {
        digest.to_vec()
    };
    let mut out = [0u8; 32];
    pbkdf2_hmac::<Sha256>(&material, salt, iterations, &mut out);
    out
}

pub struct SrpSecrets {
    pub a_priv: Vec<u8>,
    pub a_pub: Vec<u8>,
}

pub fn start_srp() -> SrpSecrets {
    let mut a_priv = vec![0u8; 256];
    rand::rngs::OsRng.fill_bytes(&mut a_priv);
    a_priv[0] |= 0x80;
    let client = SrpClient::<Sha256>::new(&G_2048);
    let a_pub = client.compute_public_ephemeral(&a_priv);
    SrpSecrets { a_priv, a_pub }
}

pub struct SrpProof {
    pub m1: Vec<u8>,
    pub m2: Vec<u8>,
}

/// Client evidence for Apple's idmsa login.
///
/// The password key is already PBKDF2'd. Apple then uses RFC 5054 with the
/// username left out of `x`, so `x = H(salt | H(":" | derived))`. The proof is
/// `M1 = H(H(N) xor H(g) | H(username) | salt | A | B | K)` with `K = H(S)`.
pub fn finish_srp(
    username: &str,
    derived_password: &[u8],
    a_priv: &[u8],
    salt: &[u8],
    b_pub: &[u8],
) -> Result<SrpProof> {
    let client = SrpClient::<Sha256>::new(&G_2048);
    let a = BigUint::from_bytes_be(a_priv);
    let b = BigUint::from_bytes_be(b_pub);
    if &b % &G_2048.n == BigUint::from(0u8) {
        return Err(ProtoError::Message("Apple sent an unusable sign-in key".into()));
    }
    let a_pub = client.compute_a_pub(&a).to_bytes_be();
    let b_bytes = b.to_bytes_be();
    let width = G_2048.n.to_bytes_be().len();
    let u = BigUint::from_bytes_be(&hash_padded(&[&a_pub, &b_bytes], width));
    let k = compute_k::<Sha256>(&G_2048);
    let x = apple_x(salt, derived_password);
    let premaster = client.compute_premaster_secret(&b, &k, &x, &a, &u);
    let session_key = Sha256::digest(premaster.to_bytes_be());
    let m1 = apple_m1(username, salt, &a_pub, &b_bytes, &session_key);
    let mut m2_hash = Sha256::new();
    m2_hash.update(&a_pub);
    m2_hash.update(&m1);
    m2_hash.update(session_key);
    Ok(SrpProof {
        m1,
        m2: m2_hash.finalize().to_vec(),
    })
}

fn apple_x(salt: &[u8], derived_password: &[u8]) -> BigUint {
    let mut inner = Sha256::new();
    inner.update(b":");
    inner.update(derived_password);
    let mut outer = Sha256::new();
    outer.update(salt);
    outer.update(inner.finalize());
    BigUint::from_bytes_be(&outer.finalize())
}

fn hash_padded(parts: &[&[u8]], width: usize) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for part in parts {
        if part.len() < width {
            hasher.update(vec![0u8; width - part.len()]);
        }
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn apple_m1(username: &str, salt: &[u8], a_pub: &[u8], b_pub: &[u8], session_key: &[u8]) -> Vec<u8> {
    let n_bytes = G_2048.n.to_bytes_be();
    let g_bytes = G_2048.g.to_bytes_be();
    let mut g_padded = vec![0u8; n_bytes.len()];
    g_padded[n_bytes.len() - g_bytes.len()..].copy_from_slice(&g_bytes);
    let h_n = Sha256::digest(&n_bytes);
    let h_g = Sha256::digest(&g_padded);
    let xor_h: Vec<u8> = h_n.iter().zip(h_g.iter()).map(|(n, g)| n ^ g).collect();
    let mut hasher = Sha256::new();
    hasher.update(xor_h);
    hasher.update(Sha256::digest(username.as_bytes()));
    hasher.update(salt);
    hasher.update(a_pub);
    hasher.update(b_pub);
    hasher.update(session_key);
    hasher.finalize().to_vec()
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn salt() -> Vec<u8> {
        (0..16).map(|i| [0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff][i]).collect()
    }

    #[test]
    fn s2k_matches_pbkdf2_sha256() {
        let out = derive_password("secret", &salt(), 1000, "s2k");
        assert_eq!(
            hex_encode(&out),
            "a514875785d9cb173064ab838ce21612040e2e4335a9bb0a2cb6b00cb2a0ef7b"
        );
    }

    #[test]
    fn apple_proof_omits_the_username_from_x() {
        let proof = finish_srp(
            "user@icloud.com",
            &(32..64).collect::<Vec<u8>>(),
            &(0..32).collect::<Vec<u8>>(),
            &(0..16).collect::<Vec<u8>>(),
            &hex_decode("3a649e1c155a80bbe00364d45a84e0d7f6a6e623062ab68c159d10935f1429c4ee35a31ca3974c9ccd4fca58e8fc1b61131925eca52f21ea4c525dfa454f3963bb83c1a98278e6ce349090ae4eee3e5be83dc5af907002a90e394cf073175dd5e6603d34e478698b7e1b9aeb35218a443a7757bd24641ae00e80aaf7a90e511771341c60637dba9a28102dade45fb7094093ae5290a070affccfb7a29ae8d2e05c49ffc827d916ba45ba4d58f51ad0b3cdf8a12097ba851ac40b12379bcd2719c74b6d762401e6a76bb138a44682321b0004b1ff4aa9bb74d79bc0c278f7edf08324eef5fed96304a0bf55534488fb999d6ff3cc3453bb4066442382ad4c5f23"),
        )
        .unwrap();
        assert_eq!(hex_encode(&proof.m1), "a00533033ddd5f3e3f7a329be083c61f4498fb595c5e63de038ef7228b90f2e9");
        assert_eq!(hex_encode(&proof.m2), "9db4f62107ff3d5210fc92778f1cabe098abd8d7cdc0096a76a061a96489f46a");
    }

    fn hex_decode(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn s2k_fo_hashes_the_password_before_pbkdf2() {
        let out = derive_password("secret", &salt(), 1000, "s2k_fo");
        assert_eq!(
            hex_encode(&out),
            "a46ce03f275215473b4c2e4e66a432f6c8292140dfd659ff66724e9e2fa42d4f"
        );
    }
}
