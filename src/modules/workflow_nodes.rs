use std::fs::{self, File};
use std::io::{BufWriter, Read, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};
use log::info;

use crate::common::{MAX_BINARY_INPUT_BYTES, MAX_TEXT_INPUT_BYTES, ensure_file_size_within};

// ─── Node: HTML → PDF ────────────────────────────────────────────────────────

/// Convert an HTML file to PDF using the system-installed Edge or Chrome browser.
pub(crate) fn convert_html_to_pdf(src: &Path, tgt: &Path) -> Result<()> {
    use headless_chrome::{Browser, LaunchOptionsBuilder};

    info!("html->pdf start: src={}, tgt={}", src.display(), tgt.display());

    ensure_file_size_within(src, MAX_TEXT_INPUT_BYTES)?;

    let browser_path = locate_system_browser()?;

    let options = LaunchOptionsBuilder::default()
        .path(Some(browser_path.into()))
        .headless(true)
        .build()
        .context("failed to build browser launch options")?;

    let browser = Browser::new(options).context("failed to launch headless browser")?;
    let tab = browser.new_tab().context("failed to open new browser tab")?;

    // Use the `url` crate to build a proper file:// URL from the local path.
    // This avoids the Windows UNC (\\?\) prefix that causes blank-page rendering.
    let absolute_path = src
        .canonicalize()
        .with_context(|| format!("failed to canonicalize html path: {}", src.display()))?;
    let url = url::Url::from_file_path(absolute_path)
        .map_err(|_| anyhow::anyhow!("failed to convert path to file URL: {}", src.display()))?
        .to_string();

    tab.navigate_to(&url).context("failed to navigate to html file")?;
    tab.wait_until_navigated().context("page load timed out")?;

    let pdf = tab.print_to_pdf(None).context("failed to print page to PDF")?;

    let mut file = File::create(tgt)
        .with_context(|| format!("failed to create pdf: {}", tgt.display()))?;
    file.write_all(&pdf)
        .with_context(|| format!("failed to write pdf: {}", tgt.display()))?;

    info!("html->pdf done: {}", tgt.display());
    Ok(())
}

/// Locate Edge or Chrome on the current Windows system.
fn locate_system_browser() -> Result<&'static str> {
    const EDGE_PATH: &str =
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe";
    const CHROME_PATH: &str =
        r"C:\Program Files\Google\Chrome\Application\chrome.exe";

    if Path::new(EDGE_PATH).exists() {
        Ok(EDGE_PATH)
    } else if Path::new(CHROME_PATH).exists() {
        Ok(CHROME_PATH)
    } else {
        bail!("no supported browser found; install Microsoft Edge or Google Chrome")
    }
}

// ─── Node: HTML → Markdown ───────────────────────────────────────────────────

/// Convert an HTML file to Markdown.
pub(crate) fn convert_html_to_md(src: &Path, tgt: &Path) -> Result<()> {
    use mdka::html_to_markdown;

    info!("html->md start: src={}, tgt={}", src.display(), tgt.display());

    ensure_file_size_within(src, MAX_TEXT_INPUT_BYTES)?;

    let mut buf = Vec::new();
    File::open(src)
        .with_context(|| format!("failed to open html: {}", src.display()))?
        .read_to_end(&mut buf)
        .with_context(|| format!("failed to read html: {}", src.display()))?;

    // `from_utf8_lossy` tolerates malformed bytes found in HTML from third-party sources.
    let html = String::from_utf8_lossy(&buf);
    let md = html_to_markdown(&html);

    fs::write(tgt, md)
        .with_context(|| format!("failed to write markdown: {}", tgt.display()))?;

    info!("html->md done: {}", tgt.display());
    Ok(())
}

// ─── Node: XLSX → CSV ────────────────────────────────────────────────────────

/// Convert all sheets of an Excel workbook to a single CSV file.
///
/// When the workbook contains multiple sheets each sheet is preceded by a
/// comment line (`## <sheet name>`) so the sheet boundaries remain visible
/// without losing any data.
pub(crate) fn convert_excel_to_csv(src: &Path, tgt: &Path) -> Result<()> {
    info!("xlsx->csv start: src={}, tgt={}", src.display(), tgt.display());

    let sheets = read_excel_sheets(src)?;

    let file = File::create(tgt)
        .with_context(|| format!("failed to create csv: {}", tgt.display()))?;
    let mut writer = BufWriter::new(file);

    let multi_sheet = sheets.len() > 1;
    for (sheet_name, grid) in &sheets {
        if multi_sheet {
            writeln!(writer, "## {sheet_name}")
                .with_context(|| format!("failed to write csv sheet header: {}", tgt.display()))?;
        }
        write_csv_rows(&grid, &mut writer)
            .with_context(|| format!("failed to write csv rows for sheet '{sheet_name}': {}", tgt.display()))?;
    }

    writer
        .flush()
        .with_context(|| format!("failed to flush csv: {}", tgt.display()))?;

    info!("xlsx->csv done: {}", tgt.display());
    Ok(())
}

// ─── Utility: Read Excel Sheets ──────────────────────────────────────────────

/// Read all non-empty sheets of an Excel workbook into an in-memory grid.
///
/// Returns `(sheet_name, rows)` pairs in workbook order.  Empty sheets are
/// skipped.  Returns an error when the workbook contains no readable data.
pub(crate) fn read_excel_sheets(src: &Path) -> Result<Vec<(String, Vec<Vec<String>>)>> {
    use calamine::{Reader, open_workbook_auto};

    ensure_file_size_within(src, MAX_BINARY_INPUT_BYTES)?;

    let mut workbook = open_workbook_auto(src)
        .with_context(|| format!("failed to open excel workbook: {}", src.display()))?;

    let mut sheets: Vec<(String, Vec<Vec<String>>)> = Vec::new();
    for sheet_name in workbook.sheet_names().to_vec() {
        if let Ok(range) = workbook.worksheet_range(&sheet_name) {
            if range.is_empty() {
                continue;
            }
            let grid: Vec<Vec<String>> = range
                .rows()
                .map(|row| row.iter().map(|cell| cell.to_string()).collect())
                .collect();
            sheets.push((sheet_name, grid));
        }
    }

    if sheets.is_empty() {
        bail!("excel workbook contains no readable sheet data: {}", src.display());
    }

    Ok(sheets)
}

// ─── Utility: Write CSV Rows ─────────────────────────────────────────────────

/// Write a two-dimensional grid of string cells as RFC 4180 CSV to `writer`.
///
/// Cells containing commas, double-quotes, or newlines are quoted and escaped.
/// Row lines are terminated with `\r\n` as required by the CSV standard.
pub(crate) fn write_csv_rows<T, W>(rows: &[Vec<T>], writer: &mut W) -> Result<()>
where
    T: AsRef<str>,
    W: Write,
{
    for row in rows {
        let mut first = true;
        for cell in row {
            if !first {
                writer.write_all(b",")?;
            }
            first = false;

            let s = cell.as_ref();
            if s.contains([',', '"', '\n', '\r']) {
                let escaped = s.replace('"', "\"\"");
                writer.write_all(b"\"")?;
                writer.write_all(escaped.as_bytes())?;
                writer.write_all(b"\"")?;
            } else {
                writer.write_all(s.as_bytes())?;
            }
        }
        writer.write_all(b"\r\n")?;
    }
    Ok(())
}

