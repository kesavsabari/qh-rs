//! The team lock. Every member has a long-term X25519 key pair (kept in config, 0600); its
//! public half is published in their board folder. The team shares one random 32-byte key; a
//! member who holds it "wraps" a copy for each other member (X25519 between the two members'
//! keys → HKDF-SHA256 → ChaCha20-Poly1305), so only that member can open it. Everything else
//! on the board is sealed with the team key, so on a public repo outsiders see noise.
use crate::oauth::{b64url, b64url_decode, random};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, CHACHA20_POLY1305};
use x25519_dalek::{PublicKey, StaticSecret};

pub type Key = [u8; 32];

fn aead(key: &Key) -> LessSafeKey {
    LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, key).expect("32-byte key"))
}

/// nonce ‖ ciphertext ‖ tag, base64url. `aad` binds the blob to what it is (and for whom).
pub fn seal(key: &Key, aad: &str, plain: &[u8]) -> String {
    let n: [u8; 12] = random()[..12].try_into().unwrap();
    let mut buf = plain.to_vec();
    aead(key).seal_in_place_append_tag(Nonce::assume_unique_for_key(n), Aad::from(aad.as_bytes()), &mut buf)
        .expect("seal");
    let mut out = n.to_vec();
    out.extend(buf);
    b64url(&out)
}

pub fn open(key: &Key, aad: &str, sealed: &str) -> Option<Vec<u8>> {
    let raw = b64url_decode(sealed.trim());
    if raw.len() < 12 + 16 {
        return None;
    }
    let (n, ct) = raw.split_at(12);
    let mut buf = ct.to_vec();
    let plain = aead(key).open_in_place(Nonce::try_assume_unique_for_key(n).ok()?, Aad::from(aad.as_bytes()), &mut buf).ok()?;
    Some(plain.to_vec())
}

/// The shared secret between two members, stretched into an encryption key.
fn pair_key(mine: &StaticSecret, theirs: &str) -> Option<Key> {
    let pk: [u8; 32] = b64url_decode(theirs).try_into().ok()?;
    let shared = mine.diffie_hellman(&PublicKey::from(pk));
    let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, b"qh team wrap v1");
    let mut out = [0u8; 32];
    salt.extract(shared.as_bytes()).expand(&[], ring::hkdf::HKDF_SHA256).ok()?.fill(&mut out).ok()?;
    Some(out)
}

/// A copy of the team key that only `their_pub`'s owner can open.
pub fn wrap(team: &Key, mine: &StaticSecret, their_pub: &str, repo: &str) -> Option<String> {
    Some(seal(&pair_key(mine, their_pub)?, &format!("wrap:{repo}"), team))
}

pub fn unwrap(blob: &str, mine: &StaticSecret, sender_pub: &str, repo: &str) -> Option<Key> {
    open(&pair_key(mine, sender_pub)?, &format!("wrap:{repo}"), blob)?.try_into().ok()
}

pub fn new_team_key() -> Key {
    random()
}

/// This machine's member key pair, created on first use.
pub fn identity() -> (StaticSecret, String) {
    let p = crate::config::config_dir().join("team-identity.key");
    let secret = std::fs::read_to_string(&p).ok()
        .and_then(|s| <[u8; 32]>::try_from(b64url_decode(s.trim())).ok())
        .map(StaticSecret::from)
        .unwrap_or_else(|| {
            let s = StaticSecret::from(random());
            let _ = std::fs::create_dir_all(crate::config::config_dir());
            let _ = crate::platform::write_private(&p, b64url(s.as_bytes()).as_bytes());
            s
        });
    let public = b64url(PublicKey::from(&secret).as_bytes());
    (secret, public)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_recipient_opens_a_wrapped_key() {
        let (a, b, c) = (StaticSecret::from(random()), StaticSecret::from(random()), StaticSecret::from(random()));
        let pub_of = |s: &StaticSecret| b64url(PublicKey::from(s).as_bytes());
        let team = new_team_key();
        let w = wrap(&team, &a, &pub_of(&b), "o/r").unwrap();
        assert_eq!(unwrap(&w, &b, &pub_of(&a), "o/r"), Some(team), "b opens what a wrapped for b");
        assert_eq!(unwrap(&w, &c, &pub_of(&a), "o/r"), None, "c can't");
        assert_eq!(unwrap(&w, &b, &pub_of(&a), "other/repo"), None, "bound to the repo");
    }

    #[test]
    fn sealed_data_needs_the_key_and_the_label() {
        let k = new_team_key();
        let s = seal(&k, "data:rahul", b"hello");
        assert_eq!(open(&k, "data:rahul", &s).as_deref(), Some(&b"hello"[..]));
        assert!(open(&k, "data:kesav", &s).is_none(), "a blob can't be passed off as someone else's");
        assert!(open(&new_team_key(), "data:rahul", &s).is_none());
    }
}
