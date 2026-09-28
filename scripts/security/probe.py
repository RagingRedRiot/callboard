#!/usr/bin/env python3
"""Raw HTTP client: deliberately bypass callboard's client-side UID checks."""
import errno
import json
import os
import socket
import sys

mode, path, uid = sys.argv[1:]
assert os.getuid() == os.geteuid() == int(uid), 'probe must run as the requested real UID'
assert os.getgid() == os.getegid() == int(uid)
status = dict(line.split(':', 1) for line in open('/proc/self/status'))
assert int(status['CapEff'].strip(), 16) == 0
assert status['NoNewPrivs'].strip() == '1'
s = socket.socket(socket.AF_UNIX)
s.settimeout(3)
try:
    s.connect(path)
except PermissionError as exc:
    assert mode == 'filesystem-denied' and exc.errno == errno.EACCES
    print('PASS: filesystem refused foreign user')
    sys.exit(0)
assert mode != 'filesystem-denied', 'foreign user connected through private permissions'
# A connected foreign socket is essential: an inaccessible socket does not
# exercise the server-side SO_PEERCRED check.
body = b'{"items":[{"key":"intruder","title":"unauthorized write"}]}'
request = (b'PUT /feeds/private HTTP/1.1\r\nHost: localhost\r\n'
           b'X-User-UID: 10001\r\nAuthorization: Bearer owner\r\n'
           b'Content-Type: application/json\r\nConnection: close\r\n'
           + f'Content-Length: {len(body)}\r\n\r\n'.encode() + body)
if mode in {'allowed', 'uid-denied-read'}:
    request = b'GET /feeds/private HTTP/1.1\r\nHost: localhost\r\nX-User-UID: 10001\r\nAuthorization: Bearer owner\r\nConnection: close\r\n\r\n'
response = bytearray()
try:
    s.sendall(request)
    while chunk := s.recv(65536):
        response.extend(chunk)
        assert len(response) < 65536, 'unexpected oversized response'
except (BrokenPipeError, ConnectionResetError):
    assert mode in {'uid-denied-read', 'uid-denied-write'}, 'owner connection was rejected'
# Timeouts, failed connects, and any other errors are failures, not rejection.
if mode in {'uid-denied-read', 'uid-denied-write'}:
    assert not response, f'foreign UID received HTTP bytes: {response!r}'
    print('PASS: connected foreign UID rejected before HTTP despite forged headers')
else:
    assert mode == 'allowed'
    assert response.startswith(b'HTTP/1.1 200 '), response
    data = json.loads(response.split(b'\r\n\r\n', 1)[1])
    assert data['items'] == [{'key': 'owner', 'title': 'private data'}], data
    print('PASS: owner read private data; foreign request did not mutate it')
