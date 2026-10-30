# qh-link

qh-link is the direct, encrypted line between teammates. Messages, notes, claims and review
requests go over it in well under a second. Without it they wait for the next git sync. This page
covers version 1 of the wire format, how two teammates find each other (the ladder), and the
crypto. The code is `src/link.rs` (the line), `src/relay.rs` (`qh relay`) and `src/tls.rs` (the
TCP lane's TLS).

qh-link knows nothing about teams. It is given our member key, each teammate's public key and the
addresses they published, and it hands back messages. When nothing gets through, a send reports
Failed and the caller falls back to git.

## Keys and names

| Name | What it is |
|---|---|
| member key | Each member's long-term X25519 key pair. The public half is `pub` in their `refs/qh/peers/<login>` folder. |
| `static_dh` | X25519(our member secret, their member public): the pair's shared secret. |
| id | `SHA-256(member public)[..8]`. Never sent in the clear. |
| `HKDF(ikm, info)` | HKDF-SHA256 with salt `"qh-link v1"`, 32 bytes out. |
| hello key | `HKDF(static_dh, "hello")`, ChaCha20-Poly1305. One per pair. |
| ephemeral key | A fresh X25519 key pair each time qh starts. |
| session keys | Made from a hello (see below). Each direction has its own key. |
| relay key of the day | Ed25519 key pair with seed `HKDF(static_dh, "relay" ‖ day)`. `day` = UTC days since 1970, as u64 big-endian. |
| relay token | `SHA-256(relay key public)[..16]`. |

Integers on the wire are big-endian.

## Packets

Every UDP packet starts with a type byte. Nothing outside the sealed part names anyone.

### Hello `0x51`

```
0x51 ‖ eph_pub(32) ‖ nonce(12) ‖ seal(hello key, aad = 0x51 ‖ eph_pub,
                                      time_ms(8) ‖ to_id(8) ‖ is_reply(1)) ‖ tag(16)
```

- The receiver finds out who sent it by trying each teammate's hello key. It accepts the hello only
  if the body opens, is 17 bytes, and `to_id` is its own id. Our own hellos coming back through
  the LAN beacon fail that last check.
- A hello is accepted only if its `time_ms` is within 10 minutes of our clock and not older than
  the last hello taken from that teammate. The same ephemeral key with the same time is a replay
  and does nothing.
- A hello with `is_reply = 0` gets a reply hello on the same path.
- Trial decryption costs one AEAD open per teammate, so strangers are rate-limited: 50 hellos a
  second per source IP, and 500 a second across all addresses where we've never seen a teammate.

### Session from a hello

When a hello carries an ephemeral key we haven't seen from that teammate:

```
ee   = X25519(our eph secret, their eph pub)
ikm  = static_dh ‖ ee
out  = our_id ‖ their_id ‖ our_eph_pub ‖ their_eph_pub
in   = their_id ‖ our_id ‖ their_eph_pub ‖ our_eph_pub
send = HKDF(ikm, out)      recv = HKDF(ikm, in)
tx_tag = HKDF(ikm, "tag" ‖ out)[..8]      rx_tag = HKDF(ikm, "tag" ‖ in)[..8]
```

The member keys make the session ours; the ephemeral keys make it new each time either side
restarts. A new session clears the teammate's half-received messages, and the first thing each
side sends on it is an ADDRS frame.

### Data `0x52`

```
0x52 ‖ tx_tag(8) ‖ counter(8) ‖ ChaCha20-Poly1305(send key, nonce = 0⁴ ‖ counter, aad = first 17 bytes, frame) ‖ tag(16)
```

- The receiver finds the session by `rx_tag`.
- `counter` starts at 1 and goes up by one per packet. Received counters are checked against a
  64-packet window, and anything already seen or older is dropped as a replay.
- If a data packet doesn't open, the sender has probably restarted, so we send a fresh hello on
  that path, at most every 2 s.

### Frames inside data

The first byte of the plaintext is the frame type.

| Byte | Frame | Body |
|---|---|---|
| 1 | FRAG | `msg_id(8) ‖ part(2) ‖ count(2) ‖ bytes` (at most 900 bytes, so a relayed fragment still fits in 1200). |
| 2 | ACK | `msg_id(8) ‖ part(2)`. |
| 3 | PING | Empty. An older keepalive; arriving was the point. |
| 5 | FWD | `to_id(8) ‖ packet`: "pass this sealed packet to that teammate". |
| 6 | FWDED | `from_id(8) ‖ packet`: "this packet came from them, through me". |
| 7 | ADDRS | Text: `ms=<sender clock> <addr> <addr> … relay=<ip:port>[/tcp][#secret] … [nat=strict]`. |
| 8 | CHECK | `nonce(8)`: "is this really you, on this path?" |
| 9 | CHECKED | `nonce(8)`, sent back the way the check came. |
| 10 | ECHO | `time_ms(8)`: a keepalive that asks for its 8 bytes back. |
| 11 | ECHOED | The same 8 bytes, back. This gives the round-trip time (smoothed 7/8 old + 1/8 new). |

Unknown frame types are ignored, which is how new frames stay compatible with older qh.

**Messages.** A message is at most 256 KiB, split into FRAGs. Every fragment is acked. Unacked
fragments are resent with backoff (×1.5, up to 2 s), and the whole message gives up after 10 s
with Failed. A receiver holds at most 16 half-received messages, or 512 KiB, per teammate. Past
that it doesn't ack, so the sender tries again later or uses git. Received message ids are
remembered (the last 512), so a resent message isn't delivered twice.

**ADDRS.** A list with an older `ms` than the last one taken is ignored. `nat=strict` means the
sender's router gives a different public port per destination (two STUN servers saw different
ports for the same IP).

## Paths, best first

| Rank | Path |
|---|---|
| 0 | Same network: private, loopback or link-local IPv4; link-local or ULA IPv6. |
| 1 | IPv6 on the internet. |
| 2 | Public IPv4 (through the NAT). |
| 3 | Through a teammate who reaches both of us (FWD/FWDED). |
| 4 | Through a `qh relay` server. |

A line moves to a new path when packets arrive there from:
- a better kind of path,
- the same kind (they moved, or their router remapped them),
- or any path, once the current one has gone quiet for longer than a keepalive.

It moves only after a CHECK sent on the new path comes back on it as CHECKED. Someone who copied
a packet and sent it on from elsewhere can't move the line.

## The ladder: how two teammates find each other

1. **Publish.** Each side puts its addresses in the `link` list of its peer folder in git: its LAN
   address, IPv6, a port mapped with UPnP (renewed every 30 min, asked for an hour) and the public
   address STUN sees (`stun.l.google.com:19302`, `stun.cloudflare.com:3478`, again every 60 s).
   It also lists its relay servers. A line that's up also carries the same list as ADDRS.
2. **LAN beacon.** Hellos for teammates we have no line to also go to multicast
   `239.255.81.72:47231` with TTL 1, so people on one network find each other with no addresses at
   all.
3. **Hello everywhere.** Send hellos to every published and remembered address at once (the last
   4 endpoints per teammate are kept). Both sides doing this at the same time is the hole punch.
   They go every second for the first 15 s, then every 5 s up to 1 min, then every 25 s up to 4 h,
   then every 5 min. 25 s is just under the 30 s Linux forgets an unanswered UDP flow, so our side
   of the hole stays open for a teammate who left.
4. **Teammate relay.** After 3 s with no line, hellos also go through every teammate who has a
   direct line to both. A relaying teammate passes only packets that came to it directly, never
   one that was already passed on (one hop). It allows 2 MiB a minute per teammate. It can't read
   what it passes.
5. **Relay server.** After 6 s (3 s when no teammate could pass it on), hellos also go to our
   relay servers and the ones the teammate published (see below). The head start lets a teammate's
   path come up first. A line that is up through a relay server tries teammates again every 30 s,
   and moves to one as soon as it answers. The default is `ovh-ca` (`15.235.127.134:47232`).
6. **TCP 443.** A relay that stays silent on UDP for 5 s is tried over TCP (443 unless the server
   line says `/port`), and TCP is retried every 60 s after that. Each packet on the stream is a
   2-byte length followed by the packet. The client opens TLS on the stream, so the lane looks
   like HTTPS. The relay also still takes plain framing.
7. **Birthday punch.** Tried when exactly one side is strict, there's no direct line, the line
   has been tried for 3 s (or is up through a relay), and the strict side published a public
   address. At most every 5 min, for 20 s:
   - The strict side opens 256 sockets toward the other side's public address.
   - The other side sprays hellos at random ports on the strict side's IP, 100 a second, until
     one lands in an open hole.
   - Once a hole works, the strict side answers through the socket that opened it.
   Two strict routers can't be punched this way. They need a relay.
8. **Up.**
   - Keepalive ECHO every 15 s.
   - A line silent for 40 s is down, and the ladder starts again.
   - After a network change (a new local address, checked every 2 s), hellos go every second for
     30 s while the line is up.
   - Messages to a teammate with no line at all fail, and the caller uses git.

## Relay server (`qh relay`)

Two teammates meet at a relay under a token only the pair can compute. The token changes every
day. Within 10 minutes of UTC midnight, both today's and the neighbouring day's token are sent, so
two clocks a little apart still meet.

```
client → relay:  0x53 ‖ token(16) ‖ mac(8) ‖ packet
relay → client:  0x54 ‖ token(16) ‖ cookie(16)                       challenge
client → relay:  0x53 ‖ token ‖ mac ‖ 0x55 ‖ side(1) ‖ cookie(16) ‖ relay key pub(32) ‖ Ed25519 sig(64)
```

- `mac` is `HMAC-SHA256(relay secret, token)[..8]` for a private relay (the default), and zeros
  for a `--public` one. The secret travels as `#<base64url>` in the server line.
- `cookie` is `HMAC(server key, token ‖ address ‖ 30 s window)[..16]`. The relay accepts this
  window's cookie or the previous one.
- `side` is 1 if our id sorts above theirs, else 0, so the two of a pair always take different
  sides.
- The signature covers `"qh relay proof v1" ‖ token ‖ cookie ‖ side`.
- The relay checks that the key hashes to the token and that the signature is good. Only then
  does it pass packets to that address.

A relay can't be used to bounce traffic at someone, and someone who only saw a token can't take
over a pair's place. The relay passes packets through untouched, so it sees IP addresses and
sizes, never content or identities. Its limits:
- 2 MiB a minute per token,
- 4 challenges a second per IP and 2000 a second overall,
- 256 tokens per IP and 200k in total,
- an address that's quiet for 2 min is forgotten.

The TCP lane's TLS adds no trust. Every packet inside is already sealed end to end and proven to
the relay. So the relay makes a throwaway self-signed Ed25519 certificate at start, and the client
accepts any certificate.

## What a network observer sees

- Hellos: a type byte, a random-looking ephemeral key, and ciphertext.
- Data: a per-session tag that changes with every session, plus a counter.
- Relay traffic: a token that changes every day.

There are no names, ids or member keys on the wire. An observer can tell that qh-link is in use
(fixed type bytes, the default ports 47232 and 47231) and see who talks to whom by IP address. The
wire doesn't try to hide either.

## Constants

| Constant | Value |
|---|---|
| Fragment size / message cap | 900 B / 256 KiB |
| Give up on a message | 10 s |
| Keepalive / line dead | 15 s / 40 s |
| Teammate relay / relay server after | 3 s / 6 s (3 s with no teammate to pass it on) |
| TCP after a silent relay / TCP retry | 5 s / 60 s |
| STUN again / network check | 60 s / 2 s |
| Birthday: sockets, rate, length, every | 256, 100/s, 20 s, 5 min |
| Relay budget (teammate or server) | 2 MiB/min |
| Clock skew allowed on hellos | 10 min |
| Relay port (UDP / TCP) | 47232 / 443 |
| LAN beacon | 239.255.81.72:47231, TTL 1 |
