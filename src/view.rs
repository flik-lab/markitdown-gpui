//! The main window: a file list on the left, the Markdown of the selected file on the right.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use gpui::{
    AnyElement, ClipboardItem, Context, Div, ExternalPaths, FontWeight, Hsla, ListAlignment,
    ListState, ObjectFit, PathPromptOptions, SharedString, Stateful, StyledImage, Window, div, img,
    list, prelude::*, px, rgb,
};

use crate::converter::{Conversion, Converter};

enum Status {
    Converting,
    Done(Arc<Conversion>),
    Failed(SharedString),
}

struct Item {
    path: PathBuf,
    status: Status,
    saved_to: Option<PathBuf>,
}

impl Item {
    fn file_name(&self) -> SharedString {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into()
    }

    fn conversion(&self) -> Option<&Arc<Conversion>> {
        match &self.status {
            Status::Done(c) => Some(c),
            _ => None,
        }
    }
}

enum BackendState {
    Starting,
    Ready(Converter),
    Missing(SharedString),
}

/// One row of the preview: a Markdown line, a line that is only an image (shown large),
/// or a line mixing text and images (e.g. a table row; images shown as thumbnails).
enum PreviewLine {
    Text(SharedString),
    Image { path: PathBuf, alt: SharedString },
    Mixed(Vec<Segment>),
}

enum Segment {
    Text(SharedString),
    Image { path: PathBuf, alt: SharedString },
}

pub struct ConverterView {
    backend: BackendState,
    items: Vec<Item>,
    selected: Option<usize>,
    preview_lines: Vec<PreviewLine>,
    preview: ListState,
    notice: Option<SharedString>,
}

// Palette (dark).
const BG: u32 = 0x1e1f22;
const PANEL: u32 = 0x2b2d31;
const BORDER: u32 = 0x3c3f45;
const TEXT: u32 = 0xdcdde0;
const MUTED: u32 = 0x8b8e94;
const ACCENT: u32 = 0x4c8dff;
const OK: u32 = 0x4fbf73;
const ERR: u32 = 0xe5534b;
const WARN: u32 = 0xe5a83b;

impl ConverterView {
    /// `initial` are files passed on the command line; they convert once the worker is up.
    pub fn new(initial: Vec<PathBuf>, cx: &mut Context<Self>) -> Self {
        let start = cx.background_executor().spawn(async { Converter::start() });
        cx.spawn(async move |this, cx| {
            let result = start.await;
            this.update(cx, |this, cx| {
                this.backend = match result {
                    Ok(c) => BackendState::Ready(c),
                    Err(e) => BackendState::Missing(format!("{e:#}").into()),
                };
                // Files added while starting were listed as "converting"; start them now.
                let pending: Vec<usize> = (0..this.items.len())
                    .filter(|&ix| matches!(this.items[ix].status, Status::Converting))
                    .collect();
                for ix in pending {
                    this.convert(ix, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();

        let mut this = Self {
            backend: BackendState::Starting,
            items: Vec::new(),
            selected: None,
            preview_lines: Vec::new(),
            preview: ListState::new(0, ListAlignment::Top, px(600.)),
            notice: None,
        };
        // Show the files right away as "converting"; they start once the worker is ready.
        this.add_files(initial, cx);
        this
    }

    fn open_files(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Convert".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update(cx, |this, cx| this.add_files(paths, cx)).ok();
            }
        })
        .detach();
    }

    fn add_files(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let converter = match &self.backend {
            BackendState::Ready(c) => Some(c.clone()),
            BackendState::Starting => None,
            BackendState::Missing(_) => return,
        };

        for path in paths.into_iter().filter(|p| p.is_file()) {
            // Adding a file that is already listed is a no-op.
            if self.items.iter().any(|i| i.path == path) {
                continue;
            }
            let ix = self.items.len();
            self.items.push(Item { path, status: Status::Converting, saved_to: None });
            if converter.is_some() {
                self.convert(ix, cx);
            }
        }
        if self.selected.is_none() && !self.items.is_empty() {
            self.select(0, cx);
        }
        cx.notify();
    }

    fn convert(&mut self, ix: usize, cx: &mut Context<Self>) {
        let BackendState::Ready(converter) = &self.backend else { return };
        let converter = converter.clone();
        let path = self.items[ix].path.clone();
        self.items[ix].status = Status::Converting;
        cx.spawn(async move |this, cx| {
            let result = converter.convert(path).await;
            this.update(cx, |this, cx| {
                let Some(item) = this.items.get_mut(ix) else { return };
                item.status = match result {
                    Ok(c) => Status::Done(Arc::new(c)),
                    Err(e) => Status::Failed(format!("{e:#}").into()),
                };
                if this.selected == Some(ix) {
                    this.refresh_preview();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn retry(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else { return };
        self.convert(ix, cx);
        self.refresh_preview();
        cx.notify();
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        self.notice = None;
        self.refresh_preview();
        cx.notify();
    }

    fn refresh_preview(&mut self) {
        self.preview_lines = self
            .selected
            .and_then(|ix| self.items.get(ix))
            .and_then(Item::conversion)
            .map(|c| c.markdown.lines().map(|l| preview_line(l, &c.dir)).collect())
            .unwrap_or_default();
        self.preview.reset(self.preview_lines.len());
    }

    fn selected_item(&self) -> Option<&Item> {
        self.selected.and_then(|ix| self.items.get(ix))
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(c) = self.selected_item().and_then(Item::conversion) {
            cx.write_to_clipboard(ClipboardItem::new_string(c.markdown.clone()));
            self.notice = Some(if c.images.is_empty() {
                "Copied Markdown to clipboard".into()
            } else {
                "Copied Markdown — image links are relative; use Save to keep the images".into()
            });
            cx.notify();
        }
    }

    fn save_as(&mut self, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else { return };
        let Some(item) = self.items.get(ix) else { return };
        let Some(conversion) = item.conversion().cloned() else { return };
        let dir = item.path.parent().map(Path::to_path_buf).unwrap_or_default();
        let name = md_path_for(&item.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let rx = cx.prompt_for_new_path(&dir, name.as_deref());
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(target))) = rx.await {
                let result = conversion.save_to(&target);
                this.update(cx, |this, cx| this.after_save(ix, target, result, cx)).ok();
            }
        })
        .detach();
    }

    /// Writes every converted file as `<name>.md` (plus its images folder) next to its source.
    fn save_all(&mut self, cx: &mut Context<Self>) {
        let mut saved = 0;
        let mut errors = Vec::new();
        for item in &mut self.items {
            let Some(c) = item.conversion() else { continue };
            let target = md_path_for(&item.path);
            match c.save_to(&target) {
                Ok(()) => {
                    item.saved_to = Some(target);
                    saved += 1;
                }
                Err(e) => errors.push(format!("{}: {e:#}", item.file_name())),
            }
        }
        self.notice = Some(if errors.is_empty() {
            format!("Saved {saved} file(s) next to their sources").into()
        } else {
            format!("Saved {saved}; failed: {}", errors.join("; ")).into()
        });
        cx.notify();
    }

    fn after_save(&mut self, ix: usize, target: PathBuf, result: anyhow::Result<()>, cx: &mut Context<Self>) {
        self.notice = Some(match result {
            Ok(()) => {
                let msg = format!("Saved {}", target.display());
                if let Some(item) = self.items.get_mut(ix) {
                    item.saved_to = Some(target);
                }
                msg.into()
            }
            Err(e) => format!("Save failed: {e:#}").into(),
        });
        cx.notify();
    }

    fn reveal(&mut self, cx: &mut Context<Self>) {
        if let Some(path) = self.selected_item().and_then(|i| i.saved_to.clone()) {
            cx.reveal_path(&path);
        }
    }

    fn clear(&mut self, cx: &mut Context<Self>) {
        self.items.clear();
        self.selected = None;
        self.notice = None;
        self.refresh_preview();
        cx.notify();
    }

    // ---------- rendering ----------

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let item = self.selected_item();
        let has_md = item.and_then(Item::conversion).is_some();
        let failed = matches!(item.map(|i| &i.status), Some(Status::Failed(_)));
        let any_done = self.items.iter().any(|i| i.conversion().is_some());
        let can_open = !matches!(self.backend, BackendState::Missing(_));

        div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .bg(rgb(PANEL))
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(div().font_weight(FontWeight::BOLD).mr_3().child("MarkItDown"))
            .child(button("open", "Open files…", can_open, true).on_click(cx.listener(|this, _, _, cx| this.open_files(cx))))
            .child(button("copy", "Copy", has_md, false).on_click(cx.listener(|this, _, _, cx| this.copy(cx))))
            .child(button("save", "Save as…", has_md, false).on_click(cx.listener(|this, _, _, cx| this.save_as(cx))))
            .child(button("save-all", "Save all beside source", any_done, false).on_click(cx.listener(|this, _, _, cx| this.save_all(cx))))
            .when(failed, |d| {
                d.child(button("retry", "Retry", true, false).on_click(cx.listener(|this, _, _, cx| this.retry(cx))))
            })
            .when(item.is_some_and(|i| i.saved_to.is_some()), |d| {
                d.child(button("reveal", "Show in folder", true, false).on_click(cx.listener(|this, _, _, cx| this.reveal(cx))))
            })
            .child(div().flex_1())
            .child(button("clear", "Clear", !self.items.is_empty(), false).on_click(cx.listener(|this, _, _, cx| this.clear(cx))))
    }

    fn render_file_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut col = div()
            .id("files")
            .flex()
            .flex_col()
            .w(px(280.))
            .h_full()
            .overflow_y_scroll()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .p_1();

        if self.items.is_empty() {
            col = col.child(div().p_3().text_sm().text_color(rgb(MUTED)).child("No files yet"));
        }

        for (ix, item) in self.items.iter().enumerate() {
            let (badge, color): (&str, u32) = match &item.status {
                Status::Converting => ("…", MUTED),
                Status::Done(c) if c.warning.is_some() => ("⚠", WARN),
                Status::Done(_) => ("✓", OK),
                Status::Failed(_) => ("✕", ERR),
            };
            let images = item.conversion().map_or(0, |c| c.images.len());
            let selected = self.selected == Some(ix);
            col = col.child(
                div()
                    .id(("file", ix))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .when(selected, |d| d.bg(rgb(0x35373c)))
                    .hover(|s| s.bg(rgb(0x313338)))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx)))
                    .child(div().w(px(14.)).text_color(rgb(color)).child(badge))
                    .child(div().flex_1().truncate().text_sm().child(item.file_name()))
                    .when(images > 0, |d| {
                        d.child(div().text_xs().text_color(rgb(MUTED)).child(format!("{images} img")))
                    }),
            );
        }
        col
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        if let BackendState::Missing(msg) = &self.backend {
            return centered(msg.clone(), ERR).into_any_element();
        }
        let Some(item) = self.selected_item() else {
            return drop_hint().into_any_element();
        };
        match &item.status {
            Status::Converting => {
                let msg = if matches!(self.backend, BackendState::Starting) {
                    "Starting MarkItDown (first start takes a few seconds)…".to_string()
                } else {
                    format!("Converting {}…", item.file_name())
                };
                centered(msg.into(), MUTED).into_any_element()
            }
            Status::Failed(err) => centered(format!("Could not convert {}\n\n{err}", item.file_name()).into(), ERR).into_any_element(),
            Status::Done(c) => div()
                .size_full()
                .flex()
                .flex_col()
                .when_some(c.warning.clone(), |d, warning| {
                    d.child(
                        div()
                            .mx_3()
                            .mt_3()
                            .px_3()
                            .py_2()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(WARN))
                            .bg(rgb(0x3a2f1c))
                            .text_color(rgb(WARN))
                            .font_family("Segoe UI")
                            .child(format!("⚠ {warning}")),
                    )
                })
                .child(div().flex_1().min_h_0().child(self.render_preview_list(cx)))
                .into_any_element(),
        }
    }

    fn render_preview_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        list(
                self.preview.clone(),
                cx.processor(|this, ix: usize, _window, _cx| match this.preview_lines.get(ix) {
                    Some(PreviewLine::Image { path, alt }) => render_image(path.clone(), alt.clone()),
                    Some(PreviewLine::Mixed(segments)) => render_mixed(segments),
                    Some(PreviewLine::Text(line)) => {
                        let heading = line.starts_with('#');
                        div()
                            .px_4()
                            .min_h(px(20.))
                            .when(heading, |d| d.text_color(rgb(ACCENT)).font_weight(FontWeight::BOLD))
                            .child(line.clone())
                            .into_any_element()
                    }
                    None => div().into_any_element(),
                }),
            )
            .size_full()
            .py_3()
    }

    fn status_text(&self) -> SharedString {
        if let Some(notice) = &self.notice {
            return notice.clone();
        }
        match &self.backend {
            BackendState::Starting => "Starting MarkItDown…".into(),
            BackendState::Missing(_) => "MarkItDown not available".into(),
            BackendState::Ready(c) => {
                let done = self.items.iter().filter(|i| i.conversion().is_some()).count();
                format!("{}   ·   {done}/{} converted", c.describe(), self.items.len()).into()
            }
        }
    }
}

impl Render for ConverterView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("root")
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Segoe UI")
            .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(rgb(0x23293a)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.notice = None;
                this.add_files(paths.paths().to_vec(), cx);
            }))
            .child(self.render_toolbar(cx))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_file_list(cx))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .overflow_hidden()
                            .font_family("Consolas")
                            .text_sm()
                            .child(self.render_main(cx)),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .bg(rgb(PANEL))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .child(self.status_text()),
            )
    }
}

/// Splits a Markdown line on local `![alt](path)` image links.
fn preview_line(line: &str, dir: &Path) -> PreviewLine {
    let mut segments = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("![") {
        let after = &rest[start + 2..];
        let Some(mid) = after.find("](") else { break };
        let Some(close) = after[mid + 2..].find(')') else { break };
        let alt = &after[..mid];
        let target = &after[mid + 2..mid + 2 + close];
        let path = dir.join(target);
        let local = !target.contains("://") && !target.starts_with("data:") && path.is_file();
        let consumed = start + 2 + mid + 2 + close + 1;
        if local {
            if start > 0 {
                segments.push(Segment::Text(rest[..start].to_owned().into()));
            }
            segments.push(Segment::Image { path, alt: alt.to_owned().into() });
        } else {
            segments.push(Segment::Text(rest[..consumed].to_owned().into()));
        }
        rest = &rest[consumed..];
    }
    if !rest.is_empty() {
        segments.push(Segment::Text(rest.to_owned().into()));
    }

    let images = segments.iter().filter(|s| matches!(s, Segment::Image { .. })).count();
    let only_image = images == 1
        && segments.iter().all(|s| match s {
            Segment::Image { .. } => true,
            Segment::Text(t) => t.trim().is_empty(),
        });
    if images == 0 {
        PreviewLine::Text(line.to_owned().into())
    } else if only_image {
        let Some(Segment::Image { path, alt }) = segments.into_iter().find(|s| matches!(s, Segment::Image { .. })) else {
            unreachable!()
        };
        PreviewLine::Image { path, alt }
    } else {
        PreviewLine::Mixed(segments)
    }
}

fn render_mixed(segments: &[Segment]) -> AnyElement {
    let mut row = div().px_4().py_1().flex().flex_wrap().items_center().gap_x_1().gap_y_1();
    for segment in segments {
        row = match segment {
            Segment::Text(t) => row.child(div().child(t.clone())),
            Segment::Image { path, .. } => row.child(
                img(path.clone())
                    .max_w(px(320.))
                    .max_h(px(200.))
                    .object_fit(ObjectFit::ScaleDown)
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(BORDER)),
            ),
        };
    }
    row.into_any_element()
}

fn render_image(path: PathBuf, alt: SharedString) -> AnyElement {
    let caption: SharedString = if alt.is_empty() {
        path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default().into()
    } else {
        alt
    };
    let fallback_caption = caption.clone();
    div()
        .px_4()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            img(path)
                .max_w(px(720.))
                .max_h(px(480.))
                .object_fit(ObjectFit::ScaleDown)
                .rounded_md()
                .with_fallback(move || {
                    div()
                        .p_2()
                        .border_1()
                        .border_dashed()
                        .border_color(rgb(BORDER))
                        .text_color(rgb(MUTED))
                        .child(format!("[image: {fallback_caption} — preview not supported]"))
                        .into_any_element()
                }),
        )
        .child(div().text_xs().text_color(rgb(MUTED)).child(caption))
        .into_any_element()
}

fn button(id: &'static str, label: &'static str, enabled: bool, primary: bool) -> Stateful<Div> {
    let base = div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .border_1()
        .border_color(rgb(BORDER));
    if !enabled {
        return base.text_color(rgb(0x5c5f66)).child(label);
    }
    let bg: Hsla = if primary { rgb(ACCENT).into() } else { rgb(0x383a40).into() };
    base.bg(bg)
        .cursor_pointer()
        .hover(|s| s.opacity(0.85))
        .active(|s| s.opacity(0.7))
        .child(label)
}

fn centered(text: SharedString, color: u32) -> impl IntoElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_6()
        .text_color(rgb(color))
        .child(text)
}

fn drop_hint() -> impl IntoElement {
    div().size_full().p_6().child(
        div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2()
            .border_2()
            .border_dashed()
            .border_color(rgb(BORDER))
            .rounded_lg()
            .font_family("Segoe UI")
            .child(div().text_xl().child("Drop files here"))
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child("Word, PowerPoint, Excel, PDF, HTML, CSV, JSON, XML, images, EPUB, ZIP…"),
            ),
    )
}

/// `report.pdf` → `report.md`; a source that is already `.md` becomes `name.converted.md`.
fn md_path_for(source: &Path) -> PathBuf {
    let is_md = source
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"));
    if is_md {
        source.with_extension("converted.md")
    } else {
        source.with_extension("md")
    }
}
