"""Protocol failures must invalidate a benchmark sample."""

from pathlib import Path
import socket
import subprocess
import tempfile
import threading
import unittest

HERE = Path(__file__).resolve().parent
BUILD = HERE.parents[1] / "target" / "comparison"
LOAD = HERE / "target" / "release" / "load"
RESPONSE = (b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n"
            b"Connection: Upgrade\r\n"
            b"Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\n")


def exact(stream, size):
    result = b""
    while len(result) < size:
        chunk = stream.recv(size - len(result))
        if not chunk:
            raise EOFError
        result += chunk
    return result


class LoadValidation(unittest.TestCase):
    def rejected(self, response, echo, expected):
        with tempfile.TemporaryDirectory(prefix="test-", dir=BUILD) as directory:
            address = str(Path(directory) / "ws.sock")
            with socket.socket(socket.AF_UNIX) as listener:
                listener.bind(address)
                listener.listen(1)
                listener.settimeout(5)
                errors = []

                def serve():
                    try:
                        with listener.accept()[0] as peer:
                            peer.settimeout(5)
                            request = b""
                            while not request.endswith(b"\r\n\r\n"):
                                request += exact(peer, 1)
                            peer.sendall(response)
                            if echo is not None:
                                # One masked 20-byte request with a short header.
                                exact(peer, 26)
                                peer.sendall(echo)
                    except Exception as error:
                        errors.append(error)

                thread = threading.Thread(target=serve)
                thread.start()
                result = subprocess.run([str(LOAD), f"unix:{address}", "1", "20", "1", "0", "0.01", "binary"],
                                        capture_output=True, text=True, timeout=10)
                thread.join(timeout=6)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors, [])
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(expected, result.stderr)
                self.assertEqual(result.stdout, "")

    def test_bad_accept_key(self):
        self.rejected(RESPONSE.replace(b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo=", b"invalid"), None, "invalid accept key")

    def test_corrupted_echo(self):
        self.rejected(RESPONSE, b"\x82\x14" + b"x" * 20, "corrupted echo")

    def test_masked_server_frame(self):
        self.rejected(RESPONSE, b"\x82\x94" + bytes(24), "invalid server frame flags")

    def test_unrequested_compression(self):
        response = RESPONSE[:-2] + b"Sec-WebSocket-Extensions: permessage-deflate\r\n\r\n"
        self.rejected(response, None, "unexpected compression")


if __name__ == "__main__":
    unittest.main()
