"""直接 mDNS 解析（1.0.2）：不依赖 adb 的 mdns 守护进程。"""

from __future__ import annotations

import socket
import struct

from tablet import mdns

SERVICE = "_adb-tls-connect._tcp.local"
INSTANCE = "adb-AM4NBB5C23001691-xn14x2 (2)._adb-tls-connect._tcp.local"
TARGET = "Android_KXJHTBDK.local"
IP = "192.0.2.10"
PORT = 44803


def _name(value: str) -> bytes:
    out = bytearray()
    for label in value.strip(".").split("."):
        raw = label.encode("ascii")
        out.append(len(raw))
        out.extend(raw)
    out.append(0)
    return bytes(out)


def _record(name: bytes, rtype: int, rdata: bytes) -> bytes:
    return name + struct.pack("!HHIH", rtype, 1, 120, len(rdata)) + rdata


def _txt(*values: str) -> bytes:
    out = bytearray()
    for value in values:
        raw = value.encode("utf-8")
        out.append(len(raw))
        out.extend(raw)
    return bytes(out)


def response(*, with_a: bool = True, compress_srv_name: bool = True) -> bytes:
    service = _name(SERVICE)
    instance = _name(INSTANCE)
    target = _name(TARGET)
    header = struct.pack("!6H", 0, 0x8400, 0, 4 if with_a else 3, 0, 0)

    ptr = _record(service, mdns._TYPE_PTR, instance)
    instance_offset = 12 + len(service) + 10
    srv_name = (
        struct.pack("!H", 0xC000 | instance_offset) if compress_srv_name else instance
    )
    srv = _record(
        srv_name,
        mdns._TYPE_SRV,
        struct.pack("!HHH", 0, 0, PORT) + target,
    )
    txt = _record(
        instance,
        mdns._TYPE_TXT,
        _txt("serial=AM4NBB5C23001691", "model=CGA-W00"),
    )
    a = _record(target, mdns._TYPE_A, socket.inet_aton(IP))
    return header + ptr + srv + txt + (a if with_a else b"")


def test_parse_response_with_compressed_srv_name() -> None:
    found = mdns.parse_response(response(), source_ip="10.0.0.1")
    assert len(found) == 1
    item = found[0]
    assert (item.ip, item.port) == (IP, PORT)
    assert item.serial == "AM4NBB5C23001691"
    assert item.model == "CGA-W00"
    assert item.target == TARGET


def test_parse_response_without_a_uses_source_ip() -> None:
    found = mdns.parse_response(response(with_a=False), source_ip=IP)
    assert [(item.ip, item.port) for item in found] == [(IP, PORT)]


def test_parse_response_deduplicates_same_endpoint() -> None:
    data = response()
    duplicate = _record(
        _name(INSTANCE),
        mdns._TYPE_SRV,
        struct.pack("!HHH", 0, 0, PORT) + _name(TARGET),
    )
    # 同一个响应里再塞一条同样的 SRV 记录，解析结果仍应只有一条。
    header = struct.pack("!6H", 0, 0x8400, 0, 5, 0, 0)
    found = mdns.parse_response(header + data[12:] + duplicate, source_ip=IP)
    assert [(item.ip, item.port) for item in found] == [(IP, PORT)]


def test_parse_response_rejects_bad_packets() -> None:
    assert mdns.parse_response(b"") == []
    assert mdns.parse_response(b"\x00" * 11) == []
    assert mdns.parse_response(b"\x00" * 12) == []
    # 查询（QR=0）不是响应，必须忽略。
    query = mdns.build_query(SERVICE)
    assert mdns.parse_response(query, IP) == []
    # 截断的响应不能抛异常。
    assert isinstance(mdns.parse_response(response()[:-40], IP), list)


def test_build_query_has_one_ptr_question() -> None:
    query = mdns.build_query(SERVICE, mdns._CLASS_IN | mdns._CLASS_QU)
    _ident, flags, qd, an, ns, ar = struct.unpack("!6H", query[:12])
    assert (flags, qd, an, ns, ar) == (0, 1, 0, 0, 0)
    assert query.endswith(struct.pack("!HH", mdns._TYPE_PTR, 0x8001))
    assert _name(SERVICE) in query
