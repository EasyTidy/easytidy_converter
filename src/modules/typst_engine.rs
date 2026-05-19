use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Result, anyhow, bail};
use image::DynamicImage;
use log::info;
use typst::LibraryExt;
use typst::diag::FileError;
use typst::foundations::Bytes as TypstBytes;
use typst::layout::PagedDocument;
use typst::syntax::{FileId, Source, VirtualPath};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;

use crate::common::{
    FileKind, MAX_TEXT_INPUT_BYTES, MAX_TYPST_PAGES, acquire_heavy_task_permit, ensure_parent_dir,
    read_text_file_limited,
};
use crate::modules::image_engine::{image_format_from_kind, save_as_webp};

static GLOBAL_FONTS: OnceLock<Vec<Font>> = OnceLock::new();

pub(crate) fn convert_md_to_pdf(src: &Path, tgt: &Path) -> Result<()> {
    info!("md->pdf start: src={}, tgt={}", src.display(), tgt.display());
    let _permit = acquire_heavy_task_permit()?;
    let _fonts = get_global_fonts()?;

    let markdown = read_text_file_limited(src, MAX_TEXT_INPUT_BYTES)?;
    let document = compile_markdown_to_typst_document(&markdown)?;

    let pdf_bytes = typst_pdf::pdf(&document, &typst_pdf::PdfOptions::default())
        .map_err(|errs| anyhow!("typst pdf export failed: {}", format_typst_diagnostics(&errs)))?;

    fs::write(tgt, pdf_bytes)
        .map_err(|e| anyhow!("failed to write pdf: {}: {e}", tgt.display()))?;
    info!("md->pdf done: {}", tgt.display());
    Ok(())
}

pub(crate) fn convert_md_to_image(src: &Path, tgt: &Path, tgt_kind: FileKind) -> Result<()> {
    info!(
        "md->image start: src={}, tgt={}, kind={:?}",
        src.display(),
        tgt.display(),
        tgt_kind
    );
    let _permit = acquire_heavy_task_permit()?;
    let _fonts = get_global_fonts()?;

    let markdown = read_text_file_limited(src, MAX_TEXT_INPUT_BYTES)?;
    let document = compile_markdown_to_typst_document(&markdown)?;

    if document.pages.is_empty() {
        bail!("typst render produced no pages")
    }
    if document.pages.len() > MAX_TYPST_PAGES {
        return Err(anyhow!("typst output page count exceeds safety limit"));
    }

    let save_page = |img: DynamicImage, out: &Path| -> Result<()> {
        if tgt_kind == FileKind::Webp {
            save_as_webp(&img, out)
        } else {
            let format = image_format_from_kind(tgt_kind)?;
            img.save_with_format(out, format)
                .map_err(|e| anyhow!("failed to write image: {}: {e}", out.display()))
        }
    };

    if document.pages.len() == 1 {
        ensure_parent_dir(tgt)?;
        let pixmap = typst_render::render(&document.pages[0], 2.0);
        let rgba = image::RgbaImage::from_raw(pixmap.width(), pixmap.height(), pixmap.data().to_vec())
            .ok_or_else(|| anyhow!("failed to create RGBA image from typst pixmap"))?;
        let img = DynamicImage::ImageRgba8(rgba);
        save_page(img, tgt)?;
    } else {
        let stem = tgt.file_stem().and_then(|s| s.to_str()).unwrap_or("page");
        let ext = tgt.extension().and_then(|s| s.to_str()).unwrap_or("png");
        let dir = tgt.parent().unwrap_or_else(|| Path::new("."));

        ensure_parent_dir(tgt)?;

        for (idx, page) in document.pages.iter().enumerate() {
            let pixmap = typst_render::render(page, 2.0);
            let rgba = image::RgbaImage::from_raw(
                pixmap.width(),
                pixmap.height(),
                pixmap.data().to_vec(),
            )
            .ok_or_else(|| anyhow!("failed to create RGBA image from typst pixmap"))?;
            let out = dir.join(format!("{stem}_page_{}.{}", idx + 1, ext));
            let img = DynamicImage::ImageRgba8(rgba);
            save_page(img, &out)?;
        }
    }

    info!("md->image done: {}", tgt.display());
    Ok(())
}

fn compile_markdown_to_typst_document(markdown: &str) -> Result<PagedDocument> {
    info!("compile markdown with typst world");
    let typst_source = markdown_to_typst(&isolate_code_blocks(markdown));
    let world = TypstMemoryWorld::new(typst_source)?;
    let warned = typst::compile::<PagedDocument>(&world);

    for warning in warned.warnings.iter() {
        info!("typst warning: {}", warning.message);
    }

    warned
        .output
        .map_err(|errs| anyhow!("typst compile failed: {}", format_typst_diagnostics(&errs)))
}

fn isolate_code_blocks(raw_md: &str) -> String {
    let normalized = raw_md.replace("\r\n", "\n");
    let mut cleaned_lines: Vec<String> = Vec::new();
    let mut in_code_block = false;

    for line in normalized.lines() {
        if line.trim_start().starts_with("```") {
            in_code_block = !in_code_block;
            cleaned_lines.push(line.to_string());
            continue;
        }

        if in_code_block {
            cleaned_lines.push(line.to_string());
        } else {
            cleaned_lines.push(escape_markdown_backslashes(line));
        }
    }

    cleaned_lines.join("\n")
}

fn escape_markdown_backslashes(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.peek().copied() {
                Some('*') | Some('_') | Some('`') | Some('#') => out.push('\\'),
                _ => out.push_str("\\\\"),
            }
        } else {
            out.push(ch);
        }
    }

    out
}

fn format_typst_diagnostics(diags: &[typst::diag::SourceDiagnostic]) -> String {
    let mut parts = Vec::with_capacity(diags.len());
    for diag in diags {
        parts.push(diag.message.to_string());
    }
    parts.join(" | ")
}

fn markdown_to_typst(markdown: &str) -> String {
    let mut out = String::from(
        "#set page(margin: 18mm)\n#set text(font: (\"Microsoft YaHei\", \"SimSun\", \"Segoe UI\", \"Libertinus Serif\"), size: 11pt)\n\n",
    );
    let mut in_code_block = false;

    for line in markdown.lines() {
        let trimmed = line.trim_start();

        if trimmed.starts_with("```") {
            in_code_block = !in_code_block;
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if in_code_block {
            out.push_str(line);
            out.push('\n');
            continue;
        }

        if trimmed.is_empty() {
            out.push('\n');
            continue;
        }

        if trimmed.starts_with("#") {
            let level = trimmed.chars().take_while(|c| *c == '#').count().clamp(1, 6);
            let title = trimmed[level..].trim();
            out.push_str(&"=".repeat(level));
            out.push(' ');
            out.push_str(&convert_inline_md_to_typst(title));
            out.push('\n');
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            out.push_str("- ");
            out.push_str(&convert_inline_md_to_typst(rest.trim()));
            out.push('\n');
            continue;
        }

        out.push_str(&convert_inline_md_to_typst(trimmed));
        out.push('\n');
    }

    out
}

fn convert_inline_md_to_typst(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut bold_open = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '*' && chars.peek().copied() == Some('*') {
            let _ = chars.next();
            out.push('*');
            bold_open = !bold_open;
            continue;
        }

        if ch == '#' {
            out.push_str("\\#");
        } else {
            out.push(ch);
        }
    }

    if bold_open {
        out.push('*');
    }

    out
}

#[cfg(test)]
mod tests {
    use super::{isolate_code_blocks, markdown_to_typst};

    #[test]
    fn isolate_code_blocks_preserves_code_blocks_and_escapes_paths() {
        let input = "```rust\r\nlet path = r#\"C:\\Users\\Downloads\"#;\r\n```\r\nWindows path: C:\\Users\\Downloads\r\nEscaped markdown: \\*bold\\* and \\#tag\r\n";

        let output = isolate_code_blocks(input);

        assert_eq!(
            output,
            "```rust\nlet path = r#\"C:\\Users\\Downloads\"#;\n```\nWindows path: C:\\\\Users\\\\Downloads\nEscaped markdown: \\*bold\\* and \\#tag"
        );
    }

    #[test]
    fn markdown_to_typst_keeps_fenced_code_block_raw() {
        let cleaned = isolate_code_blocks("```rust\n#[no_mangle]\nfn main() {}\n```\n");
        let typst = markdown_to_typst(&cleaned);

        assert!(typst.contains("```rust\n#[no_mangle]\nfn main() {}\n```"));
        assert!(!typst.contains("= [no_mangle]"));
    }
}

struct TypstMemoryWorld {
    main_id: FileId,
    source: Source,
    library: LazyHash<typst::Library>,
    book: LazyHash<FontBook>,
    fonts: &'static Vec<Font>,
}

impl TypstMemoryWorld {
    fn new(main_text: String) -> Result<Self> {
        let main_id = FileId::new(None, VirtualPath::new("/main.typ"));
        let source = Source::new(main_id, main_text);

        let fonts = get_global_fonts()?;

        let book = LazyHash::new(FontBook::from_fonts(fonts.iter()));
        let library = LazyHash::new(typst::Library::default());

        Ok(Self {
            main_id,
            source,
            library,
            book,
            fonts,
        })
    }
}

fn get_global_fonts() -> Result<&'static Vec<Font>> {
    let fonts = GLOBAL_FONTS.get_or_init(|| {
        let mut loaded = Vec::new();
        for data in typst_assets::fonts() {
            let bytes = TypstBytes::new(data.to_vec());
            loaded.extend(Font::iter(bytes));
        }

        load_windows_system_fonts(&mut loaded);
        loaded
    });

    if fonts.is_empty() {
        bail!("no typst fonts available")
    }

    Ok(fonts)
}

fn load_windows_system_fonts(fonts: &mut Vec<Font>) {
    if !cfg!(target_os = "windows") {
        return;
    }

    let mut candidate_dirs: Vec<PathBuf> = Vec::new();
    if let Ok(windir) = std::env::var("WINDIR") {
        candidate_dirs.push(Path::new(&windir).join("Fonts"));
    }
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        candidate_dirs.push(
            Path::new(&local_app_data)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts"),
        );
    }

    for dir in candidate_dirs {
        if !dir.exists() {
            continue;
        }

        let entries = match fs::read_dir(&dir) {
            Ok(v) => v,
            Err(err) => {
                info!("skip unreadable fonts directory {}: {}", dir.display(), err);
                continue;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let ext = path
                .extension()
                .and_then(|s| s.to_str())
                .map(|s| s.to_ascii_lowercase());
            let is_font_file = matches!(ext.as_deref(), Some("ttf") | Some("ttc"));
            if !is_font_file {
                continue;
            }

            match fs::read(&path) {
                Ok(data) => {
                    let before = fonts.len();
                    let bytes = TypstBytes::new(data);
                    fonts.extend(Font::iter(bytes));
                    let added = fonts.len().saturating_sub(before);
                    if added > 0 {
                        info!("loaded {} system font face(s) from {}", added, path.display());
                    }
                }
                Err(err) => {
                    info!("skip unreadable font file {}: {}", path.display(), err);
                }
            }
        }
    }
}

impl typst::World for TypstMemoryWorld {
    fn library(&self) -> &LazyHash<typst::Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.main_id
    }

    fn source(&self, id: FileId) -> typst::diag::FileResult<Source> {
        if id == self.main_id {
            Ok(self.source.clone())
        } else {
            Err(FileError::NotFound(id.vpath().as_rooted_path().to_path_buf()))
        }
    }

    fn file(&self, id: FileId) -> typst::diag::FileResult<TypstBytes> {
        Err(FileError::NotFound(id.vpath().as_rooted_path().to_path_buf()))
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.get(index).cloned()
    }

    fn today(&self, _offset: Option<i64>) -> Option<typst::foundations::Datetime> {
        None
    }
}
