"""Long-lived MarkItDown worker for markitdown-gpui.

Protocol (UTF-8 JSON, one object per line):
  stdout on start:  {"ready": true, "version": "..."}  or  {"ready": false, "error": "..."}
  stdin request:    {"id": 1, "path": "C:/x/report.docx", "out_dir": "C:/tmp/job-1"}
  stdout response:  {"id": 1, "ok": true, "markdown": "...", "images": ["report_images/img-001.png", ...],
                     "warning": null | "file was damaged; recovered N parts ..."}
                    {"id": 1, "ok": false, "error": "..."}

Images embedded as data: URIs (DOCX, PPTX, HTML, EPUB) are written to
<out_dir>/<stem>_images/ and the Markdown links are rewritten to that relative path.
PDF images are extracted with pdfminer and listed per page at the end of the document.
Truncated Office files (interrupted downloads) are rebuilt from their intact parts.
"""

import base64
import hashlib
import json
import os
import re
import sys
import traceback

sys.stdout.reconfigure(encoding="utf-8", newline="\n")
sys.stdin.reconfigure(encoding="utf-8")

# Keep stdout clean for the protocol: anything a library prints goes to stderr.
_protocol_out = sys.stdout
sys.stdout = sys.stderr


def send(obj):
    _protocol_out.write(json.dumps(obj, ensure_ascii=False) + "\n")
    _protocol_out.flush()


try:
    from markitdown import MarkItDown, __version__ as MD_VERSION

    MD = MarkItDown(enable_plugins=False)
except Exception as e:  # noqa: BLE001
    send({"ready": False, "error": f"{type(e).__name__}: {e}"})
    sys.exit(1)

DATA_URI = re.compile(r"!\[([^\]]*)\]\(data:image/([\w.+-]+);base64,([A-Za-z0-9+/=\s]+)\)")
EXT = {"jpeg": "jpg", "svg+xml": "svg", "x-emf": "emf", "x-wmf": "wmf"}


# 1x1 transparent PNG used for images lost from a damaged file; recognised again after
# conversion and turned into a "*[image lost]*" note instead of an image file.
PLACEHOLDER_PNG = base64.b64decode(
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg=="
)


def safe_stem(path):
    stem = os.path.splitext(os.path.basename(path))[0]
    return re.sub(r"[^\w\-]+", "_", stem).strip("_") or "document"


class ImageSink:
    def __init__(self, out_dir, stem):
        self.folder = f"{stem}_images"
        self.abs_dir = os.path.join(out_dir, self.folder)
        self.files = []
        self.by_hash = {}

    def _next_name(self, ext):
        return f"img-{len(self.files) + 1:03d}.{ext}"

    def add_bytes(self, data, ext):
        # The same picture used several times in a document is stored once.
        key = hashlib.sha1(data).hexdigest()
        if key in self.by_hash:
            return self.by_hash[key]
        os.makedirs(self.abs_dir, exist_ok=True)
        name = self._next_name(ext)
        with open(os.path.join(self.abs_dir, name), "wb") as f:
            f.write(data)
        rel = f"{self.folder}/{name}"
        self.files.append(rel)
        self.by_hash[key] = rel
        return rel

    def add_file(self, src_path):
        ext = os.path.splitext(src_path)[1].lstrip(".").lower() or "bin"
        name = self._next_name(ext)
        os.makedirs(self.abs_dir, exist_ok=True)
        os.replace(src_path, os.path.join(self.abs_dir, name))
        rel = f"{self.folder}/{name}"
        self.files.append(rel)
        return rel


def extract_data_uris(markdown, sink):
    def repl(m):
        alt, subtype, payload = m.group(1), m.group(2).lower(), m.group(3)
        try:
            data = base64.b64decode(re.sub(r"\s+", "", payload))
        except Exception:  # noqa: BLE001
            return m.group(0)
        if data == PLACEHOLDER_PNG:
            return f"*[image lost: {alt or 'damaged file'}]*"
        rel = sink.add_bytes(data, EXT.get(subtype, subtype))
        return f"![{alt}]({rel})"

    return DATA_URI.sub(repl, markdown)


def pdf_images(path, sink):
    """Extracts images per page with pdfminer; returns a Markdown section (or "")."""
    try:
        from pdfminer.high_level import extract_pages
        from pdfminer.image import ImageWriter
        from pdfminer.layout import LTContainer, LTImage
    except ImportError:
        return ""

    tmp = os.path.join(sink.abs_dir, "_tmp")
    os.makedirs(tmp, exist_ok=True)
    writer = ImageWriter(tmp)

    def walk(obj):
        if isinstance(obj, LTImage):
            yield obj
        elif isinstance(obj, LTContainer):
            for child in obj:
                yield from walk(child)

    sections = []
    for page_no, page in enumerate(extract_pages(path), start=1):
        links = []
        for image in walk(page):
            try:
                name = writer.export_image(image)
            except Exception:  # noqa: BLE001 - unsupported colour space / filter
                continue
            rel = sink.add_file(os.path.join(tmp, name))
            links.append(f"![Page {page_no} image]({rel})")
        if links:
            sections.append(f"### Page {page_no}\n\n" + "\n\n".join(links))
    try:
        os.rmdir(tmp)
    except OSError:
        pass
    if not sections:
        return ""
    return "\n\n## Images\n\n" + "\n\n".join(sections) + "\n"


OOXML_EXTS = (".docx", ".pptx", ".xlsx", ".docm", ".pptm", ".xlsm")


def is_valid_zip(path):
    import zipfile

    try:
        with zipfile.ZipFile(path):
            return True
    except zipfile.BadZipFile:
        return False


def repair_zip(path, out_path):
    """Rebuilds a truncated/damaged ZIP from its complete local entries.

    Truncated downloads lose the central directory at the end of the file, so the whole
    document looks unreadable even though most parts are intact. Returns (kept, lost names).
    """
    import struct
    import zipfile
    import zlib

    with open(path, "rb") as f:
        data = f.read()
    parts, lost, pos = {}, [], 0
    while True:
        i = data.find(b"PK\x03\x04", pos)
        if i < 0 or i + 30 > len(data):
            break
        _, _, flags, method, _, _, crc, csize, _, nlen, xlen = struct.unpack("<IHHHHHIIIHH", data[i : i + 30])
        name = data[i + 30 : i + 30 + nlen].decode("utf-8", "replace")
        start = i + 30 + nlen + xlen
        end = start + csize
        if flags & 0x08 or end > len(data) or method not in (0, 8):
            lost.append(name)
            pos = i + 4
            continue
        raw = data[start:end]
        try:
            body = zlib.decompress(raw, -15) if method == 8 else raw
        except zlib.error:
            body = None
        if body is None or (zlib.crc32(body) & 0xFFFFFFFF) != crc:
            lost.append(name)
            pos = i + 4
            continue
        parts[name] = body
        pos = end

    # Parts that sat after the cut are gone without a trace; find them through the
    # relationship files. Missing images get a placeholder (reported as "image lost"),
    # other missing parts have their relationship removed so the converter skips them.
    rel_re = re.compile(rb'<Relationship\b[^>]*?/>')
    for rels_name in [n for n in parts if n.endswith(".rels")]:
        base = os.path.dirname(os.path.dirname(rels_name))  # "word/_rels/x.rels" -> "word"

        def fix(m):
            tag = m.group(0)
            if b'TargetMode="External"' in tag:
                return tag
            target = re.search(rb'Target="([^"]+)"', tag)
            if not target:
                return tag
            t = target.group(1).decode("utf-8", "replace")
            full = os.path.normpath(t.lstrip("/") if t.startswith("/") else os.path.join(base, t)).replace("\\", "/")
            if full in parts:
                return tag
            if full not in lost:
                lost.append(full)
            if b"/relationships/image" in tag:
                parts[full] = PLACEHOLDER_PNG
                return tag
            return b""

        parts[rels_name] = rel_re.sub(fix, parts[rels_name])

    with zipfile.ZipFile(out_path, "w", zipfile.ZIP_DEFLATED) as out:
        for name, body in parts.items():
            out.writestr(name, body)
    kept = sum(1 for b in parts.values() if b is not PLACEHOLDER_PNG)
    return kept, lost


def convert(req):
    path, out_dir = req["path"], req["out_dir"]
    os.makedirs(out_dir, exist_ok=True)
    sink = ImageSink(out_dir, safe_stem(path))
    warning = None

    source = path
    ext = os.path.splitext(path)[1].lower()
    if ext in OOXML_EXTS and not is_valid_zip(path):
        repaired = os.path.join(out_dir, "_repaired" + ext)
        kept, lost = repair_zip(path, repaired)
        if kept == 0:
            raise ValueError(
                "This is not a valid Office file (no readable parts). "
                "It may be password-protected, encrypted by a sensitivity label, or not a real " + ext + " file."
            )
        source = repaired
        shown = ", ".join(lost[:5]) + (f" and {len(lost) - 5} more" if len(lost) > 5 else "")
        warning = (
            f"The file is incomplete or damaged (likely an interrupted download/copy). "
            f"Recovered {kept} parts; lost {len(lost)}" + (f": {shown}" if lost else "") + "."
        )

    result = MD.convert(source, keep_data_uris=True)
    markdown = extract_data_uris(result.text_content or "", sink)
    if ext == ".pdf":
        markdown = markdown.rstrip() + "\n" + pdf_images(path, sink)
    if source != path:
        try:
            os.remove(source)
        except OSError:
            pass
    return {"ok": True, "markdown": markdown, "images": sink.files, "warning": warning}


send({"ready": True, "version": MD_VERSION})

for line in sys.stdin:
    line = line.strip().lstrip("﻿")
    if not line:
        continue
    req_id = None
    try:
        req = json.loads(line)
        req_id = req.get("id")
        resp = convert(req)
    except Exception as e:  # noqa: BLE001
        traceback.print_exc(file=sys.stderr)
        resp = {"ok": False, "error": f"{type(e).__name__}: {e}"}
    resp["id"] = req_id
    send(resp)
