use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result};
use image::DynamicImage;
use log::info;
use pdf_oxide::converters::ConversionOptions as PdfConversionOptions;
use pdfium_render::prelude::PdfRenderConfig;

use crate::common::{
    EngineError, FileKind, MAX_BINARY_INPUT_BYTES, MAX_PDF_PAGES, acquire_heavy_task_permit,
    ensure_file_size_within, ensure_parent_dir,
};
use crate::{get_pdfium_instance, PDFIUM_RENDER_LOCK};
use crate::modules::image_engine::{image_format_from_kind, save_as_gif, save_as_webp};

pub(crate) fn convert_pdf_to_markdown(src: &Path, tgt: &Path) -> Result<()> {
    info!("pdf->md start: src={}, tgt={}", src.display(), tgt.display());

    let _permit = acquire_heavy_task_permit()?;
    ensure_file_size_within(src, MAX_BINARY_INPUT_BYTES)?;

    let doc = pdf_oxide::PdfDocument::open(src)
        .with_context(|| format!("failed to open PDF: {}", src.display()))?;

    let page_count = doc
        .page_count()
        .with_context(|| format!("failed to read PDF page count: {}", src.display()))?;
    if page_count == 0 {
        return Err(EngineError::DecodeLimit("pdf has zero pages").into());
    }
    if page_count > MAX_PDF_PAGES {
        return Err(EngineError::DecodeLimit("pdf page count exceeds safety limit").into());
    }

    let options = PdfConversionOptions {
        detect_headings: true,
        extract_tables: true,
        include_images: false,
        embed_images: false,
        ..Default::default()
    };

    ensure_parent_dir(tgt)?;
    let file = File::create(tgt).with_context(|| format!("failed to write markdown: {}", tgt.display()))?;
    let mut writer = BufWriter::new(file);

    let mut wrote_any = false;
    for i in 0..page_count {
        let chunk = doc
            .to_markdown(i, &options)
            .with_context(|| format!("PDF page {} markdown conversion failed: {}", i + 1, src.display()))?;
        if !chunk.trim().is_empty() {
            writer
                .write_all(chunk.as_bytes())
                .with_context(|| format!("failed writing markdown page {}: {}", i + 1, tgt.display()))?;
            wrote_any = true;
        }
        if i + 1 < page_count {
            writer
                .write_all(b"\n\n---\n\n")
                .with_context(|| format!("failed writing page separator: {}", tgt.display()))?;
        }
    }
    writer.flush().with_context(|| format!("failed to flush markdown: {}", tgt.display()))?;

    if !wrote_any {
        return Err(EngineError::DecodeLimit("pdf produced empty markdown output").into());
    }

    info!("pdf->md done: {}", tgt.display());
    Ok(())
}

pub(crate) fn convert_pdf_to_image(src: &Path, tgt: &Path, tgt_kind: FileKind) -> Result<()> {
    info!(
        "pdf->image start: src={}, tgt={}, kind={:?}",
        src.display(),
        tgt.display(),
        tgt_kind
    );

    let _permit = acquire_heavy_task_permit()?;
    ensure_file_size_within(src, MAX_BINARY_INPUT_BYTES)?;
    let pdfium = get_pdfium_instance()?;

    // pdfium 原生库非线程安全，且 pdfium-render 不会对 FFI 调用加锁。
    // 全程持锁，串行化文档加载与逐页渲染，避免并发任务触发 0xC0000005。
    // 锁保护的数据是 ()，持锁期间若 panic 导致中毒也可安全恢复。
    let _render_guard = PDFIUM_RENDER_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let doc = pdfium
        .load_pdf_from_file(src, None)
        .with_context(|| format!("failed to open PDF for rendering: {}", src.display()))?;

    let page_count = doc.pages().len() as usize;
    if page_count == 0 {
        anyhow::bail!("PDF has no pages: {}", src.display());
    }
    if page_count > MAX_PDF_PAGES {
        return Err(EngineError::DecodeLimit("pdf page count exceeds safety limit").into());
    }

    let render_config = PdfRenderConfig::new()
        .set_target_width(2480)
        .set_maximum_height(3508);

    let save_page = |img: DynamicImage, out: &Path| -> Result<()> {
        if tgt_kind == FileKind::Webp {
            save_as_webp(&img, out)
        } else if tgt_kind == FileKind::Gif {
            save_as_gif(&img, out)
        } else {
            let fmt = image_format_from_kind(tgt_kind)?;
            img.save_with_format(out, fmt)
                .with_context(|| format!("failed to save image: {}", out.display()))
        }
    };

    if page_count == 1 {
        ensure_parent_dir(tgt)?;
        let page = doc
            .pages()
            .get(0)
            .with_context(|| "failed to access PDF page 0")?;
        let img = page
            .render_with_config(&render_config)
            .with_context(|| "failed to render PDF page 0")?
            .as_image()
            .with_context(|| "failed to convert rendered page 0 into image")?;
        save_page(img, tgt)?;
    } else {
        let stem = tgt.file_stem().and_then(|s| s.to_str()).unwrap_or("page");
        let ext = tgt.extension().and_then(|s| s.to_str()).unwrap_or("png");
        let dir = tgt.parent().unwrap_or_else(|| Path::new("."));

        ensure_parent_dir(tgt)?;

        for i in 0..page_count {
            let page = doc
                .pages()
                .get(i as i32)
                .with_context(|| format!("failed to access PDF page {i}"))?;
            let img = page
                .render_with_config(&render_config)
                .with_context(|| format!("failed to render PDF page {i}"))?
                .as_image()
                .with_context(|| format!("failed to convert rendered page {i} into image"))?;
            let out = dir.join(format!("{stem}_page_{}.{}", i + 1, ext));
            save_page(img, &out)?;
            info!("pdf->image rendered page {}/{}: {}", i + 1, page_count, out.display());
        }
    }

    info!("pdf->image done: src={}, {} page(s)", src.display(), page_count);
    Ok(())
}
