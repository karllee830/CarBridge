// SPDX-FileCopyrightText: 2026 DiPlay contributors
// SPDX-FileCopyrightText: 2026 xcertplay contributors
// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: GPL-3.0-only
//
// Modified by CarBridge contributors on 2026-11-20: rewritten in Rust from DiPlay's airplay/Srp6a.kt (DiPlay commit
// 615b32c); also accepts client public keys with a leading zero byte however the client hashes them.

//! The server side of SRP-6a as AirPlay's pair-setup uses it: the 3072-bit group of RFC 5054, generator 5,
//! SHA-512, the user name `Pair-Setup` and the setup code as password.
//!
//! The hashes take the public keys padded to the group's 384 bytes, the session key is the hash of the shared
//! secret without leading zeros, and `H(N) xor H(g)` hashes both unpadded.
//!
//! About one public key in 256 is a byte shorter than the group. Clients differ in how they send and hash such a
//! key: padded, as sent, or without its leading zero. [`Server::verify`] tries each form the client's key can
//! take, in the scrambling parameter and in the proofs, and answers with the form the client's proof used. The
//! server draws its own private value again until its public key has no leading zero, so that its key reads the
//! same in every form.

use std::sync::OnceLock;

use num_bigint::BigUint;

use crate::crypto::{constant_time_eq, random, sha512};

/// The user name of pair-setup.
pub const USERNAME: &str = "Pair-Setup";
/// The length of the group's numbers, and of the padded public keys.
pub const GROUP_BYTES: usize = 384;
const SALT_BYTES: usize = 16;
const PRIVATE_BYTES: usize = 32;
const GENERATOR: u32 = 5;

/// The prime of RFC 5054's 3072-bit group.
const PRIME_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74",
    "020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F1437",
    "4FE1356D6D51C245E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3DC2007CB8A163BF05",
    "98DA48361C55D39A69163FA8FD24CF5F83655D23DCA3AD961C62F356208552BB",
    "9ED529077096966D670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF695581718",
    "3995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33",
    "A85521ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864",
    "D87602733EC86A64521F2B18177B200CBBE117577A615D6C770988C0BAD946E2",
    "08E24FA074E5AB3143DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);

/// The group's constants, worked out once.
struct Group {
    prime: BigUint,
    generator: BigUint,
    /// `k = H(N | pad(g))`.
    multiplier: BigUint,
    /// `H(N) xor H(g)`.
    prime_xor_generator: [u8; 64],
}

fn group() -> &'static Group {
    static GROUP: OnceLock<Group> = OnceLock::new();
    GROUP.get_or_init(|| {
        let prime = BigUint::parse_bytes(PRIME_HEX.as_bytes(), 16).expect("the prime is hex");
        let generator = BigUint::from(GENERATOR);
        let multiplier = number(&sha512(&[&prime.to_bytes_be(), &pad(&generator)]));
        let hash_prime = sha512(&[&prime.to_bytes_be()]);
        let hash_generator = sha512(&[&generator.to_bytes_be()]);
        let mut prime_xor_generator = [0; 64];
        for (out, (p, g)) in prime_xor_generator
            .iter_mut()
            .zip(hash_prime.iter().zip(hash_generator))
        {
            *out = p ^ g;
        }
        Group {
            prime,
            generator,
            multiplier,
            prime_xor_generator,
        }
    })
}

fn number(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

/// A number in big-endian bytes, left-padded with zeros to the group's length.
fn pad(value: &BigUint) -> Vec<u8> {
    let bytes = value.to_bytes_be();
    if bytes.len() >= GROUP_BYTES {
        return bytes;
    }
    let mut padded = vec![0; GROUP_BYTES - bytes.len()];
    padded.extend_from_slice(&bytes);
    padded
}

/// What a successful verification gives.
#[derive(Clone, PartialEq, Eq)]
pub struct Verified {
    /// `K`, the key the rest of pair-setup derives its keys from.
    pub session_key: [u8; 64],
    /// `M2`, for the client to check.
    pub server_proof: [u8; 64],
}

impl std::fmt::Debug for Verified {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Verified { .. }")
    }
}

/// One pair-setup's SRP server: its salt and public key go to the client, which answers with its own public key
/// and proof.
pub struct Server {
    salt: [u8; SALT_BYTES],
    /// `H(I)`.
    user_hash: [u8; 64],
    verifier: BigUint,
    private: BigUint,
    /// `B`, always [`GROUP_BYTES`] long without padding.
    public: Vec<u8>,
}

impl Server {
    /// A server for `username` and `password` with a fresh salt and private value.
    pub fn new(username: &str, password: &str) -> Server {
        loop {
            if let Some(server) = Server::with_secrets(username, password, random(), random()) {
                return server;
            }
        }
    }

    /// A server with the given salt and private value, or `None` when its public key would have a leading zero.
    fn with_secrets(
        username: &str,
        password: &str,
        salt: [u8; SALT_BYTES],
        private: [u8; PRIVATE_BYTES],
    ) -> Option<Server> {
        let group = group();
        let inner = sha512(&[username.as_bytes(), b":", password.as_bytes()]);
        let exponent = number(&sha512(&[&salt, &inner]));
        let verifier = group.generator.modpow(&exponent, &group.prime);
        let private = number(&private);
        let public = (&group.multiplier * &verifier
            + group.generator.modpow(&private, &group.prime))
            % &group.prime;
        let public = public.to_bytes_be();
        (public.len() == GROUP_BYTES).then(|| Server {
            salt,
            user_hash: sha512(&[username.as_bytes()]),
            verifier,
            private,
            public,
        })
    }

    pub fn salt(&self) -> &[u8; SALT_BYTES] {
        &self.salt
    }

    /// `B`, [`GROUP_BYTES`] long.
    pub fn public_key(&self) -> &[u8] {
        &self.public
    }

    /// Checks the client's public key `A` and proof `M1`, and gives the session key and the server's proof, or
    /// `None` when the proof does not match or `A` is not acceptable.
    pub fn verify(&self, client_public: &[u8], client_proof: &[u8]) -> Option<Verified> {
        let group = group();
        let first = client_public.iter().position(|&byte| byte != 0)?;
        let minimal = &client_public[first..];
        if minimal.len() > GROUP_BYTES {
            return None;
        }
        let a = number(minimal);
        if (&a % &group.prime) == BigUint::ZERO {
            return None;
        }
        let mut forms: Vec<Vec<u8>> = Vec::with_capacity(3);
        for form in [pad(&a), client_public.to_vec(), minimal.to_vec()] {
            if !forms.contains(&form) {
                forms.push(form);
            }
        }
        for scrambled in &forms {
            let u = number(&sha512(&[scrambled, &self.public]));
            if u == BigUint::ZERO {
                continue;
            }
            let base = (&a * self.verifier.modpow(&u, &group.prime)) % &group.prime;
            let secret = base.modpow(&self.private, &group.prime);
            let unpadded = secret.to_bytes_be();
            let padded = pad(&secret);
            let secrets: &[&[u8]] = if unpadded == padded {
                &[&unpadded]
            } else {
                &[&unpadded, &padded]
            };
            for secret in secrets {
                let session_key = sha512(&[secret]);
                for proved in &forms {
                    let expected = sha512(&[
                        &group.prime_xor_generator,
                        &self.user_hash,
                        &self.salt,
                        proved,
                        &self.public,
                        &session_key,
                    ]);
                    if constant_time_eq(&expected, client_proof) {
                        let server_proof = sha512(&[proved, client_proof, &session_key]);
                        return Some(Verified {
                            session_key,
                            server_proof,
                        });
                    }
                }
            }
        }
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::crypto::{from_hex, to_hex};

    const PASSWORD: &str = "3939";
    const SALT: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
    /// A private value whose public key is a full 384 bytes.
    const SERVER_PRIVATE: &str = "0da01052b2a1fb56b13d53d42ae0855a8d3d14a3375c3245d14075b7356c252e";
    /// A private value whose public key has a leading zero, with that salt and password.
    const SERVER_PRIVATE_SHORT: &str =
        "d2dc8834b3f1edb4281eafb5404e86a0ed54ad6da741bcbdeec85ff40c79ba5d";
    pub(crate) const CLIENT_PRIVATE: &str =
        "c8e114402a02f9a96be3dee8eb586414968a2aaa3e0fbdf0c776c904ecd842a8";
    /// A client private value whose public key is 383 bytes.
    pub(crate) const CLIENT_PRIVATE_SHORT: &str =
        "241a3f1a6c5e2a9ff6d9e4a2bdd529aa2fae91b52b0988a6aa6309001531fa1b";

    fn secret(text: &str) -> [u8; 32] {
        from_hex(text).try_into().unwrap()
    }

    fn server() -> Server {
        Server::with_secrets(USERNAME, PASSWORD, SALT, secret(SERVER_PRIVATE)).unwrap()
    }

    /// How a client sends and hashes its public key.
    #[derive(Clone, Copy)]
    pub(crate) enum Form {
        Padded,
        Minimal,
    }

    pub(crate) struct Client {
        pub public: Vec<u8>,
        pub proof: [u8; 64],
        pub session_key: [u8; 64],
        pub expected_server_proof: [u8; 64],
    }

    /// An SRP-6a client run against a server's salt and public key: `sent` is the form of `A` it sends,
    /// `scrambled` and `proved` the forms it hashes into `u` and into the proofs.
    pub(crate) fn client(
        salt: &[u8],
        server_public: &[u8],
        password: &str,
        private: [u8; 32],
        sent: Form,
        scrambled: Form,
        proved: Form,
    ) -> Client {
        let group = group();
        let n = &group.prime;
        let a = number(&private);
        let public = group.generator.modpow(&a, n);
        let form = |form: Form| match form {
            Form::Padded => pad(&public),
            Form::Minimal => public.to_bytes_be(),
        };
        let b = number(server_public);
        let u = number(&sha512(&[&form(scrambled), &pad(&b)]));
        let inner = sha512(&[USERNAME.as_bytes(), b":", password.as_bytes()]);
        let x = number(&sha512(&[salt, &inner]));
        let kv = (&group.multiplier * group.generator.modpow(&x, n)) % n;
        let base = (&b + n - kv) % n;
        let secret = base.modpow(&(&a + &u * &x), n);
        let session_key = sha512(&[&secret.to_bytes_be()]);
        let proof = sha512(&[
            &group.prime_xor_generator,
            &sha512(&[USERNAME.as_bytes()]),
            salt,
            &form(proved),
            &pad(&b),
            &session_key,
        ]);
        let expected_server_proof = sha512(&[&form(proved), &proof, &session_key]);
        Client {
            public: form(sent),
            proof,
            session_key,
            expected_server_proof,
        }
    }

    fn run(private: &str, sent: Form, scrambled: Form, proved: Form) -> (Client, Option<Verified>) {
        let server = server();
        let client = client(
            server.salt(),
            server.public_key(),
            PASSWORD,
            secret(private),
            sent,
            scrambled,
            proved,
        );
        let verified = server.verify(&client.public, &client.proof);
        (client, verified)
    }

    #[test]
    fn known_answer() {
        // Worked out with Python's integers and hashlib from the same secrets.
        let server = server();
        assert_eq!(
            to_hex(&server.public_key()[..16]),
            "1cbd285550c6fdf69aea0b2e77b12e8b"
        );
        let (client, verified) = run(CLIENT_PRIVATE, Form::Padded, Form::Padded, Form::Padded);
        assert_eq!(
            to_hex(&client.session_key[..16]),
            "acca5bd440774f8290b27ffd811e7b03"
        );
        assert_eq!(
            to_hex(&client.proof[..16]),
            "e3a6af9465bfbb279c756bb71959e130"
        );
        let verified = verified.unwrap();
        assert_eq!(verified.session_key, client.session_key);
        assert_eq!(
            to_hex(&verified.server_proof[..16]),
            "8ea16c7105f492e45f6f93aa14f29f7c"
        );
    }

    #[test]
    fn a_client_key_with_a_leading_zero_is_accepted_in_every_form() {
        use Form::*;
        let (padded, _) = run(CLIENT_PRIVATE_SHORT, Padded, Padded, Padded);
        assert_eq!(padded.public.len(), GROUP_BYTES);
        assert_eq!(padded.public[0], 0);
        assert_eq!(
            to_hex(&padded.proof[..16]),
            "33499630604bc82735a694f4c94d6257"
        );
        let (minimal, _) = run(CLIENT_PRIVATE_SHORT, Minimal, Minimal, Minimal);
        assert_eq!(minimal.public.len(), GROUP_BYTES - 1);
        assert_eq!(
            to_hex(&minimal.proof[..16]),
            "8e5e8e9a757c25f15f229a5db7591c00"
        );

        for sent in [Padded, Minimal] {
            for scrambled in [Padded, Minimal] {
                for proved in [Padded, Minimal] {
                    let (client, verified) = run(CLIENT_PRIVATE_SHORT, sent, scrambled, proved);
                    let verified = verified.expect("leading-zero key accepted");
                    assert_eq!(verified.session_key, client.session_key);
                    assert_eq!(verified.server_proof, client.expected_server_proof);
                }
            }
        }
    }

    #[test]
    fn a_wrong_password_or_proof_fails() {
        let server = server();
        let client = client(
            server.salt(),
            server.public_key(),
            "0000",
            secret(CLIENT_PRIVATE),
            Form::Padded,
            Form::Padded,
            Form::Padded,
        );
        assert!(server.verify(&client.public, &client.proof).is_none());
        let (mut client, _) = run(CLIENT_PRIVATE, Form::Padded, Form::Padded, Form::Padded);
        client.proof[0] ^= 1;
        assert!(server.verify(&client.public, &client.proof).is_none());
        assert!(server.verify(&client.public, &client.proof[..63]).is_none());
    }

    #[test]
    fn degenerate_client_keys_are_refused() {
        let server = server();
        let proof = [0u8; 64];
        assert!(server.verify(&[], &proof).is_none());
        assert!(server.verify(&[0; GROUP_BYTES], &proof).is_none());
        assert!(server
            .verify(&group().prime.to_bytes_be(), &proof)
            .is_none());
        assert!(server.verify(&[1; GROUP_BYTES + 1], &proof).is_none());
    }

    #[test]
    fn the_server_key_never_has_a_leading_zero() {
        assert!(
            Server::with_secrets(USERNAME, PASSWORD, SALT, secret(SERVER_PRIVATE_SHORT)).is_none()
        );
        let server = Server::new(USERNAME, PASSWORD);
        assert_eq!(server.public_key().len(), GROUP_BYTES);
        assert_ne!(server.public_key()[0], 0);
    }
}
