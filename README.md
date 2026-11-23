# MarkItDown (GPUI)

Desktop app (Rust + [GPUI](https://gpui.rs)) that converts documents to Markdown using
Microsoft's [MarkItDown](https://github.com/microsoft/markitdown), **keeping the images**.

## Requirements

- Python with MarkItDown: `python -m pip install "markitdown[all]"`
- Rust (the toolchain is pinned in `rust-toolchain.toml`; rustup installs it automatically)

## Run

```powershell
cargo run --release
cargo run --release -- report.docx slides.pptx   # convert files on launch
```

## Features

- Drag and drop files onto the window, or **Open files…**
- Images are extracted and shown inline in the preview
- **Copy**, **Save as…**, **Save all beside source** (`report.docx` → `report.md` + `report_images/`)
- **Retry** for failed files (with hints for locked / old `.doc` / damaged files), **Show in folder**

## How it works

- `src/worker.py` is embedded in the exe and started once as a long-lived Python process, so
  MarkItDown is imported only once (~6 s) instead of once per file.
- Images:
  - **DOCX, PPTX, HTML, EPUB** — converted with `keep_data_uris=True`; the base64 images are
    decoded into `<name>_images/img-001.png`, … and the Markdown links rewritten to those files.
  - **PDF** — MarkItDown only extracts text, so images are extracted with pdfminer and listed per
    page under an `## Images` section at the end (their exact position in the text is not known).
- Saving copies the `<name>_images/` folder next to the `.md`, so relative links keep working.

Set `MARKITDOWN_PYTHON` to use a specific interpreter (for example `C:\venv\Scripts\python.exe`).

## Test

```powershell
$env:MARKITDOWN_TEST_FILE = "C:\path\to\file-with-images.docx"
cargo test -- --ignored
```
