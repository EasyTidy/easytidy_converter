use std::ffi::c_char;
use std::sync::{Once, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use pdfium_render::prelude::Pdfium;

mod common;
mod modules;

pub use common::{ConvertErrorCode, FileKind};

use common::{
    c_path_from_ptr, detect_kind, is_image_kind, map_error_to_code,
};
use modules::image_engine::convert_image;
use modules::office_engine::{
    convert_docx_to_markdown, convert_docx_to_plain_text, convert_excel_to_markdown,
    convert_md_to_docx,
};
use modules::pdf_engine::{convert_pdf_to_image, convert_pdf_to_markdown};
use modules::workflow_nodes::{convert_html_to_pdf, convert_html_to_md, convert_excel_to_csv};
use modules::typst_engine::{convert_md_to_image, convert_md_to_pdf};

static LOGGER_INIT: Once = Once::new();
pub(crate) static PDFIUM_INSTANCE: OnceLock<Pdfium> = OnceLock::new();

#[unsafe(no_mangle)]
pub unsafe extern "C" fn easytidy_convert_file(src_ptr: *const c_char, tgt_ptr: *const c_char) -> i32 {
    let guard = std::panic::catch_unwind(|| unsafe {
        let src = c_path_from_ptr(src_ptr).context("invalid src path pointer")?;
        let tgt = c_path_from_ptr(tgt_ptr).context("invalid tgt path pointer")?;
        convert_file(&src, &tgt)
    });

    match guard {
        Ok(Ok(())) => ConvertErrorCode::Success as i32,
        Ok(Err(err)) => {
            let code = map_error_to_code(&err);
            eprintln!("[EasyTidyConverter] convert failed (code={code}): {err:?}");
            code
        }
        Err(_) => {
            eprintln!("[EasyTidyConverter] panic captured inside easytidy_convert_file");
            ConvertErrorCode::Panic as i32
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn easytidy_init_logger() -> i32 {
    let guard = std::panic::catch_unwind(|| {
        LOGGER_INIT.call_once(|| {
            let mut builder = env_logger::Builder::from_env(
                env_logger::Env::default().default_filter_or("info"),
            );
            builder.format_timestamp_secs();

            match builder.try_init() {
                Ok(()) => {
                    eprintln!("[EasyTidyConverter] logger initialized (env_logger)");
                }
                Err(err) => {
                    eprintln!("[EasyTidyConverter] logger already initialized or unavailable: {err}");
                }
            }
        });

        ConvertErrorCode::Success as i32
    });

    match guard {
        Ok(code) => code,
        Err(_) => ConvertErrorCode::Panic as i32,
    }
}

fn convert_file(src: &std::path::Path, tgt: &std::path::Path) -> Result<()> {
    let src_kind = detect_kind(src).with_context(|| format!("unsupported src: {}", src.display()))?;
    let tgt_kind = detect_kind(tgt).with_context(|| format!("unsupported tgt: {}", tgt.display()))?;

    match (src_kind, tgt_kind) {
        (a, b) if is_image_kind(a) && is_image_kind(b) => convert_image(src, tgt, b),
        (FileKind::Md, FileKind::Docx) => convert_md_to_docx(src, tgt),
        (FileKind::Md, FileKind::Pdf) => convert_md_to_pdf(src, tgt),
        (FileKind::Md, img) if is_image_kind(img) => convert_md_to_image(src, tgt, img),
        (FileKind::Xlsx, FileKind::Md) => convert_excel_to_markdown(src, tgt),
        (FileKind::Docx, FileKind::Md) => convert_docx_to_markdown(src, tgt),
        (FileKind::Docx, FileKind::Txt) => convert_docx_to_plain_text(src, tgt),
        (FileKind::Pdf, img) if is_image_kind(img) => convert_pdf_to_image(src, tgt, img),
        (FileKind::Pdf, FileKind::Md) => convert_pdf_to_markdown(src, tgt),
        // Node: HTML -> PDF
        (FileKind::Html, FileKind::Pdf) => convert_html_to_pdf(src, tgt),
        // Node: HTML -> Markdown
        (FileKind::Html, FileKind::Md) => convert_html_to_md(src, tgt),
        // Node: XLSX -> CSV
        (FileKind::Xlsx, FileKind::Csv) => convert_excel_to_csv(src, tgt),
        _ => bail!("unsupported conversion route: {src_kind:?} -> {tgt_kind:?}"),
    }
}

pub(crate) fn get_pdfium_instance() -> Result<&'static Pdfium> {
    if let Some(instance) = PDFIUM_INSTANCE.get() {
        return Ok(instance);
    }

    let bindings = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./"))
        .or_else(|_| Pdfium::bind_to_system_library())
        .map_err(|e| {
            anyhow!(
                "pdfium library not found ({e}); \
                 place pdfium.dll / libpdfium.so alongside the converter DLL"
            )
        })?;

    let _ = PDFIUM_INSTANCE.set(Pdfium::new(bindings));

    PDFIUM_INSTANCE
        .get()
        .context("failed to initialize global pdfium instance")
}
