//! The Noise handshake `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` and the transport it leads to.
//!
//! It follows the Noise specification (revision 34): the symmetric state of section 5.2, the
//! handshake pattern `NNpsk0` of section 9, and the pre-shared key rules of section 9.2, where
//! the token `e` also mixes the ephemeral key into the chaining key.
//!
//! | Message | Tokens |
//! |---|---|
//! | initiator to responder | `psk`, `e` |
//! | responder to initiator | `e`, `ee` |

use blake2::{Blake2s256, Digest};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hmac::{Mac, SimpleHmac};
use x25519_dalek::{PublicKey, StaticSecret};

/// The protocol name, hashed into the first handshake hash.
const NAME: &[u8] = b"Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
/// The bytes of a hash, a key and a public key alike.
const LEN: usize = 32;
/// The bytes the cipher adds to each message.
pub const TAG: usize = 16;

/// Why a message was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    /// The message is not as long as its tokens need.
    Short,
    /// The message failed to decrypt: the other end holds another key, or the bytes changed.
    Decrypt,
    /// A message out of the handshake's order.
    Order,
}

fn hash(parts: &[&[u8]]) -> [u8; LEN] {
    let mut h = Blake2s256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; LEN] {
    // HMAC accepts a key of any length, so this never fails.
    let mut m = <SimpleHmac<Blake2s256> as Mac>::new_from_slice(key).expect("any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// HKDF of section 4.3: 3 outputs from the chaining key and the input key material.
fn hkdf(ck: &[u8; LEN], ikm: &[u8]) -> [[u8; LEN]; 3] {
    let temp = hmac(ck, &[ikm]);
    let o1 = hmac(&temp, &[&[1]]);
    let o2 = hmac(&temp, &[&o1, &[2]]);
    let o3 = hmac(&temp, &[&o2, &[3]]);
    [o1, o2, o3]
}

/// The 96-bit nonce of ChaChaPoly in Noise: 4 zero bytes, then the counter little endian.
fn nonce(n: u64) -> Nonce {
    let mut b = [0u8; 12];
    b[4..].copy_from_slice(&n.to_le_bytes());
    Nonce::from(b)
}

fn encrypt(k: &[u8; LEN], n: u64, ad: &[u8], plain: &[u8]) -> Vec<u8> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*k));
    // Encryption fails only on a message too long for the cipher, which a frame never is.
    cipher
        .encrypt(
            &nonce(n),
            Payload {
                msg: plain,
                aad: ad,
            },
        )
        .expect("a frame fits the cipher")
}

fn decrypt(k: &[u8; LEN], n: u64, ad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, Error> {
    let cipher = ChaCha20Poly1305::new(&Key::from(*k));
    cipher
        .decrypt(
            &nonce(n),
            Payload {
                msg: sealed,
                aad: ad,
            },
        )
        .map_err(|_| Error::Decrypt)
}

/// The symmetric state of section 5.2, with its cipher key once one is mixed in.
struct Symmetric {
    ck: [u8; LEN],
    h: [u8; LEN],
    k: Option<[u8; LEN]>,
    n: u64,
}

impl Symmetric {
    fn new() -> Self {
        // A name longer than the hash is hashed, as section 5.2 says.
        let h = hash(&[NAME]);
        let mut s = Self {
            ck: h,
            h,
            k: None,
            n: 0,
        };
        // The prologue is empty.
        s.mix_hash(&[]);
        s
    }

    fn mix_hash(&mut self, data: &[u8]) {
        self.h = hash(&[&self.h, data]);
    }

    fn mix_key(&mut self, ikm: &[u8]) {
        let [ck, k, _] = hkdf(&self.ck, ikm);
        self.ck = ck;
        self.k = Some(k);
        self.n = 0;
    }

    fn mix_key_and_hash(&mut self, ikm: &[u8]) {
        let [ck, h, k] = hkdf(&self.ck, ikm);
        self.ck = ck;
        self.mix_hash(&h);
        self.k = Some(k);
        self.n = 0;
    }

    fn encrypt_and_hash(&mut self, plain: &[u8]) -> Vec<u8> {
        let out = match &self.k {
            Some(k) => {
                let sealed = encrypt(k, self.n, &self.h, plain);
                self.n += 1;
                sealed
            }
            None => plain.to_vec(),
        };
        self.mix_hash(&out);
        out
    }

    fn decrypt_and_hash(&mut self, data: &[u8]) -> Result<Vec<u8>, Error> {
        let plain = match &self.k {
            Some(k) => {
                let plain = decrypt(k, self.n, &self.h, data)?;
                self.n += 1;
                plain
            }
            None => data.to_vec(),
        };
        self.mix_hash(data);
        Ok(plain)
    }

    /// The 2 transport keys: the first for messages from the initiator.
    fn split(&self) -> ([u8; LEN], [u8; LEN]) {
        let [k1, k2, _] = hkdf(&self.ck, &[]);
        (k1, k2)
    }
}

/// One end of a handshake.
pub struct Handshake {
    sym: Symmetric,
    initiator: bool,
    e: StaticSecret,
    re: Option<PublicKey>,
    psk: [u8; LEN],
    /// The messages written or read so far.
    step: u8,
}

impl Handshake {
    /// A handshake with the group's pre-shared key and this end's ephemeral secret.
    pub fn new(initiator: bool, psk: [u8; LEN], ephemeral: [u8; LEN]) -> Self {
        Self {
            sym: Symmetric::new(),
            initiator,
            e: StaticSecret::from(ephemeral),
            re: None,
            psk,
            step: 0,
        }
    }

    /// The next message this end sends, carrying `payload`.
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        match (self.initiator, self.step) {
            (true, 0) => {
                self.sym.mix_key_and_hash(&self.psk);
                self.write_e(&mut out);
            }
            (false, 1) => {
                self.write_e(&mut out);
                self.mix_ee()?;
            }
            _ => return Err(Error::Order),
        }
        out.extend(self.sym.encrypt_and_hash(payload));
        self.step += 1;
        Ok(out)
    }

    /// Read the other end's next message and return its payload.
    pub fn read_message(&mut self, message: &[u8]) -> Result<Vec<u8>, Error> {
        if message.len() < LEN + TAG {
            return Err(Error::Short);
        }
        let (re, rest) = message.split_at(LEN);
        match (self.initiator, self.step) {
            (false, 0) => {
                self.sym.mix_key_and_hash(&self.psk);
                self.read_e(re);
            }
            (true, 1) => {
                self.read_e(re);
                self.mix_ee()?;
            }
            _ => return Err(Error::Order),
        }
        let payload = self.sym.decrypt_and_hash(rest)?;
        self.step += 1;
        Ok(payload)
    }

    fn write_e(&mut self, out: &mut Vec<u8>) {
        let public = PublicKey::from(&self.e).to_bytes();
        out.extend_from_slice(&public);
        self.sym.mix_hash(&public);
        // With a pre-shared key, the token `e` mixes the key too (section 9.2).
        self.sym.mix_key(&public);
    }

    fn read_e(&mut self, re: &[u8]) {
        let mut bytes = [0u8; LEN];
        bytes.copy_from_slice(re);
        self.sym.mix_hash(&bytes);
        self.sym.mix_key(&bytes);
        self.re = Some(PublicKey::from(bytes));
    }

    fn mix_ee(&mut self) -> Result<(), Error> {
        let re = self.re.ok_or(Error::Order)?;
        let shared = self.e.diffie_hellman(&re);
        self.sym.mix_key(shared.as_bytes());
        Ok(())
    }

    /// The transport, once both messages have passed.
    pub fn into_transport(self) -> Result<Transport, Error> {
        if self.step != 2 {
            return Err(Error::Order);
        }
        let (k1, k2) = self.sym.split();
        let (send, recv) = match self.initiator {
            true => (k1, k2),
            false => (k2, k1),
        };
        Ok(Transport { send, recv })
    }
}

/// The keys of a channel, each message sealed under the counter its sender gives it.
pub struct Transport {
    send: [u8; LEN],
    recv: [u8; LEN],
}

impl Transport {
    /// `plain` sealed under the counter `n`, with no associated data.
    pub fn seal(&self, n: u64, plain: &[u8]) -> Vec<u8> {
        encrypt(&self.send, n, &[], plain)
    }

    /// The plain text of `sealed` under the counter `n`.
    pub fn open(&self, n: u64, sealed: &[u8]) -> Result<Vec<u8>, Error> {
        decrypt(&self.recv, n, &[], sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PSK: [u8; 32] = [7; 32];
    const EI: [u8; 32] = [1; 32];
    const ER: [u8; 32] = [2; 32];

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    // Written by another implementation of the same protocol (snow 0.10), with the pre-shared
    // key PSK and the ephemeral secrets EI and ER: payloads "hello", "world", then "frame" sent
    // by the initiator under the counter 5.
    const M1: &str = "a4e09292b651c278b9772c569f5fa9bb13d906b46ab68c9df9dc2b4409f8a2091ea865f9ad0eded01430ecde8e8093819d52effef0";
    const M2: &str = "ce8d3ad1ccb633ec7b70c17814a5c76ecd029685050d344745ba05870e587d59c07708855e08b3e34ebc335dbb90d1d17689a8cdaf";
    const F5: &str = "367236c3a17a0ce6835fc73b7ee0b1640e0443c16f";

    /// Both ends give, byte for byte, the messages the other implementation gave.
    #[test]
    fn the_messages_match_another_implementation() {
        let mut i = Handshake::new(true, PSK, EI);
        let mut r = Handshake::new(false, PSK, ER);
        let m1 = i.write_message(b"hello").unwrap();
        assert_eq!(m1, unhex(M1));
        assert_eq!(r.read_message(&m1).unwrap(), b"hello");
        let m2 = r.write_message(b"world").unwrap();
        assert_eq!(m2, unhex(M2));
        assert_eq!(i.read_message(&m2).unwrap(), b"world");
        let (ti, tr) = (i.into_transport().unwrap(), r.into_transport().unwrap());
        assert_eq!(ti.seal(5, b"frame"), unhex(F5));
        assert_eq!(tr.open(5, &unhex(F5)).unwrap(), b"frame");
    }

    #[test]
    fn another_key_cannot_finish_the_handshake() {
        let mut i = Handshake::new(true, PSK, EI);
        let mut r = Handshake::new(false, [8; 32], ER);
        let m1 = i.write_message(b"").unwrap();
        assert_eq!(r.read_message(&m1), Err(Error::Decrypt));
    }

    #[test]
    fn a_frame_under_another_counter_is_refused() {
        let mut i = Handshake::new(true, PSK, EI);
        let mut r = Handshake::new(false, PSK, ER);
        let m1 = i.write_message(b"").unwrap();
        r.read_message(&m1).unwrap();
        let m2 = r.write_message(b"").unwrap();
        i.read_message(&m2).unwrap();
        let (ti, tr) = (i.into_transport().unwrap(), r.into_transport().unwrap());
        assert_eq!(tr.open(1, &ti.seal(0, b"up")), Err(Error::Decrypt));
        assert_eq!(ti.open(1, &tr.seal(1, b"down")).unwrap(), b"down");
    }

    #[test]
    fn a_short_or_early_message_is_refused() {
        let mut r = Handshake::new(false, PSK, ER);
        assert_eq!(r.read_message(&[0; 10]), Err(Error::Short));
        let mut i = Handshake::new(true, PSK, EI);
        assert_eq!(i.read_message(&[0; 64]), Err(Error::Order));
    }
}
