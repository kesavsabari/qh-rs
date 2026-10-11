//! The team lock. Every member has a long-term X25519 key pair (kept in config, 0600); its
//! public half is published in their board folder. The team shares one random 32-byte key; a
//! member who holds it "wraps" a copy for each other member (X25519 between the two members'
//! keys → HKDF-SHA256 → ChaCha20-Poly1305), so only that member can open it. Everything else
//! on the board is sealed with the team key, so on a public repo outsiders see noise.
//!
//! Git doesn't say who pushed a ref, so anyone with push access could write a folder under
//! someone else's name. Every folder is therefore signed (Ed25519, derived from the member key),
//! and readers only take folders whose signature matches the key they know for that login.
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

/// The member's signing key, derived from their member key (so there's still one secret to keep).
fn signer(mine: &StaticSecret) -> ring::signature::Ed25519KeyPair {
    let salt = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, b"qh team sign v1");
    let mut seed = [0u8; 32];
    salt.extract(mine.as_bytes()).expand(&[], ring::hkdf::HKDF_SHA256).expect("hkdf").fill(&mut seed).expect("hkdf");
    ring::signature::Ed25519KeyPair::from_seed_unchecked(&seed).expect("32-byte seed")
}

pub fn sign_pub(mine: &StaticSecret) -> String {
    use ring::signature::KeyPair;
    b64url(signer(mine).public_key().as_ref())
}

/// What a folder signature covers: the repo, the login, the commit message, and every file
/// (name and exact bytes) except the signature itself.
fn folder_bytes(repo: &str, login: &str, msg: &str, files: &[(String, String)]) -> Vec<u8> {
    let mut f: Vec<&(String, String)> = files.iter().filter(|(n, _)| n != "sig").collect();
    f.sort();
    let mut out = format!("qh folder v1\n{repo}\n{login}\n{}\n", msg.trim()).into_bytes();
    for (name, content) in f {
        out.extend(format!("{name}\0{}\0", content.len()).as_bytes());
        out.extend(content.as_bytes());
    }
    out
}

pub fn sign_folder(mine: &StaticSecret, repo: &str, login: &str, msg: &str, files: &[(String, String)]) -> String {
    b64url(signer(mine).sign(&folder_bytes(repo, login, msg, files)).as_ref())
}

pub fn folder_signed_by(sign_pub: &str, sig: &str, repo: &str, login: &str, msg: &str, files: &[(String, String)]) -> bool {
    let key = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, b64url_decode(sign_pub.trim()));
    key.verify(&folder_bytes(repo, login, msg, files), &b64url_decode(sig.trim())).is_ok()
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

    #[test]
    fn a_folder_signature_binds_every_file_the_login_and_the_repo() {
        let (a, m) = (StaticSecret::from(random()), StaticSecret::from(random()));
        let files = vec![("pub".to_string(), "P".to_string()), ("data".to_string(), "D".to_string()), ("keys/bob".to_string(), "W".to_string())];
        let sig = sign_folder(&a, "o/r", "alice", "{\"ts\":1}", &files);
        let pk = sign_pub(&a);
        assert_eq!(pk, sign_pub(&a), "the same member key gives the same signing key");
        assert!(folder_signed_by(&pk, &sig, "o/r", "alice", "{\"ts\":1}\n", &files), "a trailing newline from git doesn't matter");
        let mut changed = files.clone();
        changed[1].1 = "D2".into();
        assert!(!folder_signed_by(&pk, &sig, "o/r", "alice", "{\"ts\":1}", &changed), "data swapped");
        let mut more = files.clone();
        more.push(("keys/mallory".into(), "W".into()));
        assert!(!folder_signed_by(&pk, &sig, "o/r", "alice", "{\"ts\":1}", &more), "a file added");
        assert!(!folder_signed_by(&pk, &sig, "o/r", "bob", "{\"ts\":1}", &files), "another login");
        assert!(!folder_signed_by(&pk, &sig, "o/x", "alice", "{\"ts\":1}", &files), "another repo");
        assert!(!folder_signed_by(&pk, &sig, "o/r", "alice", "{\"ts\":2}", &files), "another message (an old folder replayed)");
        let forged = sign_folder(&m, "o/r", "alice", "{\"ts\":1}", &files);
        assert!(!folder_signed_by(&pk, &forged, "o/r", "alice", "{\"ts\":1}", &files), "a teammate can't sign as alice");
    }
}
