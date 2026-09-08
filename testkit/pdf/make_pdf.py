#!/usr/bin/env python3
"""生成一份带暗号的最小合法 PDF。

暗号是判据:模型只有真读到文件本体才说得出来。若 PDF 没送到、或被降级成
一句"用户发了个文件",它顶多复述文件名——那正是我们要抓的回归。

xref 表按字节偏移手算,不靠第三方库(测具不该引入依赖)。

    python3 make_pdf.py out.pdf SECRET-4Q7X
"""

import sys


def build(secret: str) -> bytes:
    text = f"BT /F1 18 Tf 40 120 Td ({secret}) Tj ET\n".encode()
    objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] "
        b"/Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>",
        b"<< /Length " + str(len(text)).encode() + b" >>\nstream\n" + text + b"endstream",
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
    ]

    out = bytearray(b"%PDF-1.4\n")
    offsets = []
    for index, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += f"{index} 0 obj\n".encode() + body + b"\nendobj\n"

    xref_at = len(out)
    out += f"xref\n0 {len(objects) + 1}\n".encode()
    out += b"0000000000 65535 f \n"
    for offset in offsets:
        out += f"{offset:010d} 00000 n \n".encode()
    out += (
        f"trailer\n<< /Size {len(objects) + 1} /Root 1 0 R >>\n"
        f"startxref\n{xref_at}\n%%EOF\n"
    ).encode()
    return bytes(out)


if __name__ == "__main__":
    path, secret = sys.argv[1], sys.argv[2]
    with open(path, "wb") as handle:
        handle.write(build(secret))
    print(f"wrote {path} ({secret})")
