#!/usr/bin/env python3
"""Minimal STUN binding server for the netsim: answers each request with XOR-MAPPED-ADDRESS."""
import socket
import struct
import sys
import threading

MAGIC = 0x2112A442


def serve(host, port):
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    s.bind((host, port))
    while True:
        pkt, (ip, p) = s.recvfrom(2048)
        if len(pkt) < 20 or struct.unpack("!HHI", pkt[:8])[0] != 1 or struct.unpack("!I", pkt[4:8])[0] != MAGIC:
            continue
        tx = pkt[8:20]
        xip = struct.unpack("!I", socket.inet_aton(ip))[0] ^ MAGIC
        attr = struct.pack("!HHBBHI", 0x0020, 8, 0, 1, p ^ (MAGIC >> 16), xip)
        s.sendto(struct.pack("!HHI", 0x0101, len(attr), MAGIC) + tx + attr, (ip, p))


host = sys.argv[1]
for port in sys.argv[2:]:
    threading.Thread(target=serve, args=(host, int(port)), daemon=True).start()
threading.Event().wait()
