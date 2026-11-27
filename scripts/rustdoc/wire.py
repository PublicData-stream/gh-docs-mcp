"""Bounded framing shared by the trusted runner and its disposable guest."""

import json
import struct


HEADER_BYTES = 16 * 1024
BUNDLE_BYTES = 256 * 1024 * 1024
JSON_BYTES = 128 * 1024 * 1024


def read_exact(reader, size):
    result = bytearray()
    while len(result) < size:
        data = reader.read(min(size - len(result), 1024 * 1024))
        if not data:
            raise EOFError("unexpected end of Rustdoc frame")
        result.extend(data)
    return result


def read_header(reader):
    size, = struct.unpack("!I", read_exact(reader, 4))
    if not 0 < size <= HEADER_BYTES:
        raise ValueError("invalid Rustdoc frame header")
    return json.loads(read_exact(reader, size))


def write_all(stream, data):
    pending = memoryview(data)
    while pending:
        count = stream.write(pending)
        if not count:
            raise EOFError("Rustdoc frame receiver disconnected")
        pending = pending[count:]


def write_header(stream, value):
    header = json.dumps(value, separators=(",", ":")).encode()
    if not 0 < len(header) <= HEADER_BYTES:
        raise ValueError("invalid Rustdoc frame header")
    write_all(stream, struct.pack("!I", len(header)))
    write_all(stream, header)
    stream.flush()


def read_body(stream, length, limit, path):
    if type(length) is not int or not 0 < length <= limit:
        raise ValueError("invalid Rustdoc frame body size")
    with path.open("xb") as output:
        while length:
            chunk = read_exact(stream, min(length, 1024 * 1024))
            output.write(chunk)
            length -= len(chunk)


def write_body(stream, path):
    with path.open("rb") as source:
        while data := source.read(1024 * 1024):
            write_all(stream, data)
    stream.flush()
