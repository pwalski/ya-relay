use crate::crypto::Crypto;
use digest::{Digest, Output};

pub const SIGNATURE_SIZE: usize = std::mem::size_of::<ethsign::Signature>();
pub const PREFIX_SIZE: usize = std::mem::size_of::<u64>();

pub async fn solve(
    challenge: &[u8],
    difficulty: u64,
    crypto: impl Crypto,
) -> anyhow::Result<Vec<u8>> {
    let solution = solve_challenge::<sha3::Sha3_512>(challenge, difficulty)?;
    sign(solution, crypto).await
}

pub fn verify(
    challenge: &[u8],
    difficulty: u64,
    response: &[u8],
    pub_key: &[u8],
) -> anyhow::Result<bool> {
    let inner = verify_signature(response, pub_key)?;
    verify_challenge::<sha3::Sha3_512>(challenge, difficulty, inner)
}

pub fn solve_challenge<D: Digest>(challenge: &[u8], difficulty: u64) -> anyhow::Result<Vec<u8>> {
    let mut counter: u64 = 0;
    loop {
        let prefix = counter.to_be_bytes();
        let result = digest::<D>(&prefix, challenge);

        if leading_zeros(&result) >= difficulty {
            let mut response = prefix.to_vec();
            response.reserve(result.len());
            response.extend(result.into_iter());
            return Ok(response);
        }

        counter = counter
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Could not find hash for difficulty {}", difficulty))?;
    }
}

pub fn verify_challenge<D: Digest>(
    challenge: &[u8],
    difficulty: u64,
    response: &[u8],
) -> anyhow::Result<bool> {
    if response.len() < PREFIX_SIZE {
        anyhow::bail!("Invalid response size: {}", response.len());
    }

    let prefix = &response[0..PREFIX_SIZE];
    let to_verify = &response[PREFIX_SIZE..];
    let expected = digest::<D>(prefix, challenge);
    let zeros = leading_zeros(expected.as_slice());

    Ok(expected.as_slice() == to_verify && zeros >= difficulty)
}

pub async fn sign(solution: Vec<u8>, crypto: impl Crypto) -> anyhow::Result<Vec<u8>> {
    let message = sha2::Sha256::digest(solution.as_slice());
    let sig = crypto.sign(message.as_slice()).await?;

    let mut result = Vec::with_capacity(SIGNATURE_SIZE + solution.len());
    result.push(sig.v);
    result.extend_from_slice(&sig.r[..]);
    result.extend_from_slice(&sig.s[..]);
    result.extend(solution.into_iter());

    Ok(result)
}

pub fn verify_signature<'b>(response: &'b [u8], pub_key: &[u8]) -> anyhow::Result<&'b [u8]> {
    let len = response.len();
    if len < SIGNATURE_SIZE {
        anyhow::bail!("Signature too short: {} out of {} B", len, SIGNATURE_SIZE);
    }

    let sig = &response[..SIGNATURE_SIZE];
    let embedded = &response[SIGNATURE_SIZE..];

    let v = sig[0];
    let mut r = [0; 32];
    let mut s = [0; 32];

    r.copy_from_slice(&sig[1..33]);
    s.copy_from_slice(&sig[33..]);

    let message = sha2::Sha256::digest(embedded);
    let recovered_key = ethsign::Signature { v, r, s }.recover(message.as_slice())?;

    if pub_key == recovered_key.bytes() {
        Ok(embedded)
    } else {
        anyhow::bail!("Invalid public key");
    }
}

fn digest<D: Digest>(nonce: &[u8], input: &[u8]) -> Output<D> {
    let mut hasher = D::new();
    hasher.update(nonce);
    hasher.update(input);
    hasher.finalize()
}

fn leading_zeros(result: &[u8]) -> u64 {
    let mut total: u64 = 0;
    for byte in result.iter() {
        if *byte == 0 {
            total += 8;
        } else {
            total += (*byte).leading_zeros() as u64;
            break;
        }
    }
    total
}
