use pbkdf2::pbkdf2_hmac;
use rand::RngCore;
use sha2::{Digest, Sha256};
use srp::client::SrpClient;
use srp::groups::G_2048;

use crate::{ProtoError, Result};

/// Apple's s2k / s2k_fo password key. This is the input to SRP, not the verifier.
pub fn derive_password(password: &str, salt: &[u8], iterations: u32, protocol: &str) -> [u8; 32] {
    let material: Vec<u8> = if protocol == "s2k_fo" {
        hex_encode(&Sha256::digest(password.as_bytes())).into_bytes()
    } else {
        password.as_bytes().to_vec()
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
    let mut a_priv = vec![0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut a_priv);
    let client = SrpClient::<Sha256>::new(&G_2048);
    let a_pub = client.compute_public_ephemeral(&a_priv);
    SrpSecrets { a_priv, a_pub }
}

pub struct SrpProof {
    pub m1: Vec<u8>,
    pub m2: Vec<u8>,
}

/// Client evidence (M1) and the server evidence Apple expects us to send (M2 = H(A | M | K)).
pub fn finish_srp(
    username: &str,
    derived_password: &[u8],
    a_priv: &[u8],
    salt: &[u8],
    b_pub: &[u8],
) -> Result<SrpProof> {
    let client = SrpClient::<Sha256>::new(&G_2048);
    let verifier = client
        .process_reply(a_priv, username.as_bytes(), derived_password, salt, b_pub)
        .map_err(|err| ProtoError::Message(format!("SRP challenge failed: {err:?}")))?;
    let a_pub = client.compute_public_ephemeral(a_priv);
    let m1 = verifier.proof().to_vec();
    let mut hasher = Sha256::new();
    hasher.update(&a_pub);
    hasher.update(&m1);
    hasher.update(verifier.key());
    Ok(SrpProof {
        m1,
        m2: hasher.finalize().to_vec(),
    })
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
            "588372775fb38b5711b6f36b465e293dc7b793e008550ee8a10bb26a8e7f85b8"
        );
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
