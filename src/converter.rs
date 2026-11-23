//! Talks to a long-lived Python process (`worker.py`) that keeps MarkItDown loaded.
//!
//! Importing MarkItDown takes ~6 s, so starting Python per file made every conversion slow.
//! The worker is started once; requests go over stdin/stdout as JSON lines and are handled
//! one at a time on a dedicated thread.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::channel::oneshot;
use serde::{Deserialize, Serialize};

const WORKER_SOURCE: &str = include_str!("worker.py");

/// Result of converting one file. Image links in `markdown` are relative to `dir`.
#[derive(Debug)]
pub struct Conversion {
    pub markdown: String,
    pub dir: PathBuf,
    /// Image paths relative to `dir`, e.g. `report_images/img-001.png`.
    pub images: Vec<String>,
    /// Set when the source was damaged and only partly recovered.
    pub warning: Option<String>,
}

impl Conversion {
    /// Writes the Markdown to `target` and copies its images next to it,
    /// so the relative links keep working.
    pub fn save_to(&self, target: &Path) -> Result<()> {
        let target_dir = target.parent().context("target has no parent folder")?;
        for rel in &self.images {
            let dest = target_dir.join(rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(self.dir.join(rel), &dest)
                .with_context(|| format!("copying image {rel}"))?;
        }
        std::fs::write(target, self.markdown.as_bytes())?;
        Ok(())
    }
}

struct Job {
    path: PathBuf,
    reply: oneshot::Sender<Result<Conversion>>,
}

/// Handle to the worker thread. Cloning is cheap; all clones share one Python process.
#[derive(Clone)]
pub struct Converter {
    jobs: mpsc::Sender<Job>,
    description: String,
}

impl Converter {
    /// Starts Python with MarkItDown loaded. Blocking (~6 s); call from a background thread.
    pub fn start() -> Result<Self> {
        let root = std::env::temp_dir().join("markitdown-gpui");
        clean_old_runs(&root);
        let run_dir = root.join(format!("run-{}", std::process::id()));
        std::fs::create_dir_all(&run_dir)?;
        let script = run_dir.join("worker.py");
        std::fs::write(&script, WORKER_SOURCE)?;

        let (process, ready) = Process::spawn_first_working(&script)?;
        let description = format!("MarkItDown {} via {}", ready.version, process.python);

        let (jobs, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("markitdown-worker".into())
            .spawn(move || worker_loop(process, script, run_dir, rx))?;

        Ok(Self { jobs, description })
    }

    pub fn describe(&self) -> &str {
        &self.description
    }

    /// Queues a conversion; resolves when the worker answers.
    pub async fn convert(&self, path: PathBuf) -> Result<Conversion> {
        let (reply, rx) = oneshot::channel();
        self.jobs
            .send(Job { path, reply })
            .map_err(|_| anyhow!("MarkItDown worker stopped"))?;
        rx.await.map_err(|_| anyhow!("MarkItDown worker stopped"))?
    }
}

fn worker_loop(mut process: Process, script: PathBuf, run_dir: PathBuf, rx: mpsc::Receiver<Job>) {
    let mut next_id: u64 = 1;
    for job in rx {
        let id = next_id;
        next_id += 1;
        let out_dir = run_dir.join(format!("job-{id}"));
        let mut result = process.request(id, &job.path, &out_dir);

        // The process may have crashed (e.g. a native library aborted). Restart once and retry.
        if matches!(&result, Err(RequestError::Io(_))) {
            if let Ok((fresh, _)) = Process::spawn(&process.python, &script) {
                process = fresh;
                result = process.request(id, &job.path, &out_dir);
            }
        }

        let result = result.map_err(|e| match e {
            RequestError::Io(e) => anyhow!("MarkItDown worker crashed: {e}"),
            RequestError::Failed(msg) => anyhow!(msg),
        });
        job.reply.send(result).ok();
    }
}

#[derive(Serialize)]
struct Request<'a> {
    id: u64,
    path: &'a str,
    out_dir: &'a str,
}

#[derive(Deserialize)]
struct Ready {
    ready: bool,
    #[serde(default)]
    version: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct Response {
    ok: bool,
    #[serde(default)]
    markdown: String,
    #[serde(default)]
    images: Vec<String>,
    #[serde(default)]
    warning: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

enum RequestError {
    Io(anyhow::Error),
    Failed(String),
}

struct Process {
    python: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for Process {
    fn drop(&mut self) {
        self.child.kill().ok();
    }
}

impl Process {
    /// Tries `MARKITDOWN_PYTHON`, then `python`, `py`, `python3`.
    fn spawn_first_working(script: &Path) -> Result<(Self, Ready)> {
        let mut candidates: Vec<String> = Vec::new();
        if let Ok(custom) = std::env::var("MARKITDOWN_PYTHON") {
            candidates.push(custom);
        }
        candidates.extend(["python", "py", "python3"].map(String::from));

        let mut errors = Vec::new();
        for python in candidates {
            match Self::spawn(&python, script) {
                Ok(ok) => return Ok(ok),
                Err(e) => errors.push(format!("{python}: {e}")),
            }
        }
        bail!(
            "MarkItDown is not available.\n\nInstall it with:\n    python -m pip install \"markitdown[all]\"\n\n{}",
            errors.join("\n")
        )
    }

    fn spawn(python: &str, script: &Path) -> Result<(Self, Ready)> {
        let mut cmd = Command::new(python);
        cmd.arg("-u")
            .arg(script)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().context("could not start")?;
        let stdin = child.stdin.take().context("no stdin")?;
        let mut stdout = BufReader::new(child.stdout.take().context("no stdout")?);

        let mut line = String::new();
        stdout.read_line(&mut line)?;
        let ready: Ready = serde_json::from_str(line.trim())
            .with_context(|| format!("unexpected output: {}", line.trim()))?;
        if !ready.ready {
            bail!(ready.error.unwrap_or_else(|| "failed to load MarkItDown".into()));
        }
        Ok((Self { python: python.to_owned(), child, stdin, stdout }, ready))
    }

    fn request(&mut self, id: u64, path: &Path, out_dir: &Path) -> Result<Conversion, RequestError> {
        let path_str = path.to_str().ok_or(RequestError::Failed("path is not valid UTF-8".into()))?;
        let out_str = out_dir.to_str().ok_or(RequestError::Failed("temp path is not valid UTF-8".into()))?;
        let mut payload = serde_json::to_string(&Request { id, path: path_str, out_dir: out_str })
            .map_err(|e| RequestError::Io(e.into()))?;
        payload.push('\n');
        self.stdin
            .write_all(payload.as_bytes())
            .and_then(|_| self.stdin.flush())
            .map_err(|e| RequestError::Io(e.into()))?;

        let mut line = String::new();
        let n = self.stdout.read_line(&mut line).map_err(|e| RequestError::Io(e.into()))?;
        if n == 0 {
            return Err(RequestError::Io(anyhow!("process exited")));
        }
        let resp: Response =
            serde_json::from_str(line.trim()).map_err(|e| RequestError::Io(e.into()))?;
        if !resp.ok {
            return Err(RequestError::Failed(friendly_error(
                resp.error.as_deref().unwrap_or("conversion failed"),
                path,
            )));
        }
        Ok(Conversion {
            markdown: resp.markdown.replace("\r\n", "\n"),
            dir: out_dir.to_path_buf(),
            images: resp.images,
            warning: resp.warning,
        })
    }
}

/// Adds a hint for the most common failures.
fn friendly_error(raw: &str, path: &Path) -> String {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let hint = if raw.contains("PermissionError") {
        "\n\nThe file is locked — close it in Word/Excel/PowerPoint and press Retry."
    } else if name.starts_with("~$") {
        "\n\nThis is an Office lock file, not the real document."
    } else if name.to_lowercase().ends_with(".doc") || name.to_lowercase().ends_with(".ppt") || name.to_lowercase().ends_with(".xls") {
        "\n\nOld binary Office formats (.doc/.ppt/.xls) are not supported. Save as .docx/.pptx/.xlsx first."
    } else if raw.contains("BadZipFile") || raw.contains("not a zip file") || raw.contains("no readable parts") {
        "\n\nThe file is damaged, password-protected, or encrypted by a sensitivity label. \
         Try opening it in Word and using Save As to make a fresh .docx."
    } else if raw.contains("MissingDependency") {
        "\n\nInstall all converters with: python -m pip install \"markitdown[all]\""
    } else {
        ""
    };
    format!("{raw}{hint}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs Python with MarkItDown, so it is opt-in:
    /// `MARKITDOWN_TEST_FILE=C:\path\file.docx cargo test -- --ignored`
    #[test]
    #[ignore]
    fn converts_and_saves_with_images() {
        let source = PathBuf::from(std::env::var("MARKITDOWN_TEST_FILE").expect("set MARKITDOWN_TEST_FILE"));
        let converter = Converter::start().expect("worker starts");
        let conversion = futures::executor::block_on(converter.convert(source)).expect("converts");
        assert!(!conversion.images.is_empty(), "expected extracted images");
        assert!(!conversion.markdown.contains("data:image"), "data URIs should be replaced");

        let out = std::env::temp_dir().join(format!("markitdown-gpui-test-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let target = out.join("result.md");
        conversion.save_to(&target).expect("saves");

        let saved = std::fs::read_to_string(&target).unwrap();
        for rel in &conversion.images {
            assert!(saved.contains(rel.as_str()), "markdown links {rel}");
            assert!(out.join(rel).is_file(), "image copied: {rel}");
        }
        std::fs::remove_dir_all(&out).ok();
    }
}

/// Removes temp folders of previous runs older than a day.
fn clean_old_runs(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    let day = Duration::from_secs(24 * 60 * 60);
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| SystemTime::now().duration_since(t).ok())
            .is_some_and(|age| age > day);
        if old {
            std::fs::remove_dir_all(entry.path()).ok();
        }
    }
}
