use std::fs;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use calamine::{Reader, open_workbook_auto};
use log::info;
use quick_xml::Reader as XmlReader;
use quick_xml::events::Event;

use crate::common::{EngineError, MAX_BINARY_INPUT_BYTES, MAX_DOCX_XML_BYTES, ensure_file_size_within};

pub(crate) fn convert_md_to_docx(src: &Path, tgt: &Path) -> Result<()> {
    use docx_rs::{Docx, Paragraph, Run};

    info!("md->docx start: src={}, tgt={}", src.display(), tgt.display());
    let markdown = fs::read_to_string(src)
        .with_context(|| format!("failed to read markdown: {}", src.display()))?;

    let mut doc = Docx::new();
    for line in markdown.lines() {
        let trimmed = line.trim_start();

        if trimmed.is_empty() {
            doc = doc.add_paragraph(Paragraph::new());
            continue;
        }

        let (heading_level, content) = parse_md_heading_line(trimmed);
        let mut paragraph = Paragraph::new();

        if let Some(level) = heading_level {
            let style_id = if level == 1 { "Heading1" } else { "Heading2" };
            paragraph = paragraph.style(style_id).outline_lvl(level.saturating_sub(1));
        }

        let runs = parse_md_inline_bold_runs(content);
        if runs.is_empty() {
            paragraph = paragraph.add_run(Run::new());
        } else {
            for (text, is_bold) in runs {
                if text.is_empty() {
                    continue;
                }
                let run = if is_bold {
                    Run::new().add_text(text).bold()
                } else {
                    Run::new().add_text(text)
                };
                paragraph = paragraph.add_run(run);
            }
        }

        doc = doc.add_paragraph(paragraph);
    }

    let file = std::fs::File::create(tgt)
        .with_context(|| format!("failed to create docx: {}", tgt.display()))?;
    doc.build().pack(file).context("failed to pack docx")?;
    info!("md->docx done: {}", tgt.display());
    Ok(())
}

fn parse_md_heading_line(line: &str) -> (Option<usize>, &str) {
    let level = line.chars().take_while(|c| *c == '#').count();
    if level == 0 {
        return (None, line);
    }

    let clamped = level.clamp(1, 2);
    let content = line[level..].trim_start();
    if content.is_empty() {
        (None, line)
    } else {
        (Some(clamped), content)
    }
}

fn parse_md_inline_bold_runs(input: &str) -> Vec<(String, bool)> {
    let mut runs = Vec::new();
    let mut buf = String::new();
    let mut bold = false;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == '*' && chars.peek().copied() == Some('*') {
            let _ = chars.next();
            runs.push((std::mem::take(&mut buf), bold));
            bold = !bold;
            continue;
        }
        buf.push(ch);
    }

    if !buf.is_empty() {
        runs.push((buf, bold));
    }

    if runs.is_empty() {
        runs.push((input.to_string(), false));
    }

    runs
}

pub(crate) fn convert_excel_to_markdown(src: &Path, tgt: &Path) -> Result<()> {
    let mut workbook = open_workbook_auto(src)
        .with_context(|| format!("failed to open excel workbook: {}", src.display()))?;

    let mut output = String::new();

    for sheet_name in workbook.sheet_names().to_vec() {
        if let Ok(range) = workbook.worksheet_range(&sheet_name) {
            if range.is_empty() {
                continue;
            }

            output.push_str("## ");
            output.push_str(&sheet_name);
            output.push_str("\n\n");

            let mut rows = range.rows();
            if let Some(header) = rows.next() {
                let header_cells: Vec<String> = header
                    .iter()
                    .map(|cell| escape_markdown_table_cell(&cell.to_string()))
                    .collect();

                if !header_cells.iter().any(|c| !c.trim().is_empty()) {
                    continue;
                }

                output.push('|');
                for cell in &header_cells {
                    output.push(' ');
                    output.push_str(cell);
                    output.push(' ');
                    output.push('|');
                }
                output.push('\n');

                output.push('|');
                for _ in &header_cells {
                    output.push_str(" --- |");
                }
                output.push('\n');
            }

            for row in rows {
                let row_cells: Vec<String> = row
                    .iter()
                    .map(|cell| escape_markdown_table_cell(&cell.to_string()))
                    .collect();

                if !row_cells.iter().any(|c| !c.trim().is_empty()) {
                    continue;
                }

                output.push('|');
                for cell in row_cells {
                    output.push(' ');
                    output.push_str(&cell);
                    output.push(' ');
                    output.push('|');
                }
                output.push('\n');
            }

            output.push('\n');
        }
    }

    if output.trim().is_empty() {
        bail!("excel workbook contains no readable sheet data")
    }

    fs::write(tgt, output).with_context(|| format!("failed to write markdown: {}", tgt.display()))?;
    Ok(())
}

pub(crate) fn escape_markdown_table_cell(input: &str) -> String {
    let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
    normalized.replace('|', "\\|").replace('\n', "<br/>")
}

pub(crate) fn convert_docx_to_markdown(src: &Path, tgt: &Path) -> Result<()> {
    info!("docx->md start: src={}, tgt={}", src.display(), tgt.display());

    ensure_file_size_within(src, MAX_BINARY_INPUT_BYTES)?;

    let file = File::open(src).with_context(|| format!("failed to open docx: {}", src.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("failed to open docx zip: {}", src.display()))?;

    let mut xml_file = archive
        .by_name("word/document.xml")
        .with_context(|| format!("word/document.xml not found: {}", src.display()))?;

    if xml_file.size() > MAX_DOCX_XML_BYTES {
        return Err(EngineError::DecodeLimit("docx xml entry too large").into());
    }

    let mut xml_bytes = Vec::with_capacity(xml_file.size().min(1024 * 1024) as usize);
    xml_file
        .read_to_end(&mut xml_bytes)
        .with_context(|| format!("failed to read word/document.xml: {}", src.display()))?;
    let xml = String::from_utf8(xml_bytes).context("word/document.xml is not valid utf-8")?;

    let markdown = extract_markdown_from_document_xml(&xml)?;
    fs::write(tgt, markdown)
        .with_context(|| format!("failed to write markdown: {}", tgt.display()))?;

    info!("docx->md done: {}", tgt.display());
    Ok(())
}

#[derive(Default)]
struct DocxParagraphAcc {
    heading_level: Option<usize>,
    runs: Vec<(String, bool)>,
    line_buf: String,
}

fn extract_markdown_from_document_xml(xml: &str) -> Result<String> {
    let mut reader = XmlReader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut out_lines: Vec<String> = Vec::new();
    let mut in_table = false;
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut buf = Vec::new();

    let mut in_paragraph = false;
    let mut in_run = false;
    let mut in_text = false;
    let mut in_run_props = false;
    let mut run_bold = false;

    let mut in_table_cell = false;
    let mut table_cell_buf = String::new();
    let mut table_row_buf: Vec<String> = Vec::new();

    let mut paragraph = DocxParagraphAcc::default();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let tag = e.name();
                if tag.as_ref() == b"w:tbl" {
                    in_table = true;
                    table_rows.clear();
                } else if in_table && tag.as_ref() == b"w:tr" {
                    table_row_buf.clear();
                } else if in_table && tag.as_ref() == b"w:tc" {
                    in_table_cell = true;
                    table_cell_buf.clear();
                } else if tag.as_ref() == b"w:p" && !in_table {
                    in_paragraph = true;
                    paragraph = DocxParagraphAcc::default();
                } else if in_paragraph && tag.as_ref() == b"w:r" {
                    in_run = true;
                    run_bold = false;
                    paragraph.line_buf.clear();
                } else if in_run && tag.as_ref() == b"w:rPr" {
                    in_run_props = true;
                } else if in_run && tag.as_ref() == b"w:t" {
                    in_text = true;
                } else if in_run_props && tag.as_ref() == b"w:b" {
                    run_bold = true;
                } else if in_paragraph && tag.as_ref() == b"w:pStyle" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            let value = String::from_utf8_lossy(attr.value.as_ref());
                            paragraph.heading_level = heading_level_from_style(&value);
                        }
                    }
                } else if in_run && tag.as_ref() == b"w:tab" {
                    paragraph.line_buf.push('\t');
                } else if in_run && tag.as_ref() == b"w:br" {
                    paragraph.line_buf.push('\n');
                }
            }
            Ok(Event::Empty(e)) => {
                let tag = e.name();
                if in_run_props && tag.as_ref() == b"w:b" {
                    run_bold = true;
                } else if in_paragraph && tag.as_ref() == b"w:pStyle" {
                    for attr in e.attributes().flatten() {
                        if attr.key.as_ref() == b"w:val" {
                            let value = String::from_utf8_lossy(attr.value.as_ref());
                            paragraph.heading_level = heading_level_from_style(&value);
                        }
                    }
                } else if in_run && tag.as_ref() == b"w:tab" {
                    paragraph.line_buf.push('\t');
                } else if in_run && tag.as_ref() == b"w:br" {
                    paragraph.line_buf.push('\n');
                }
            }
            Ok(Event::Text(t)) => {
                if in_text {
                    let text = t
                        .decode()
                        .map_err(|e| anyhow!("xml text decode failed: {e}"))?;
                    if in_table_cell {
                        table_cell_buf.push_str(text.as_ref());
                    } else {
                        paragraph.line_buf.push_str(text.as_ref());
                    }
                }
            }
            Ok(Event::End(e)) => {
                let tag = e.name();
                if tag.as_ref() == b"w:t" {
                    in_text = false;
                } else if tag.as_ref() == b"w:rPr" {
                    in_run_props = false;
                } else if tag.as_ref() == b"w:r" {
                    in_run = false;
                    if !paragraph.line_buf.is_empty() {
                        paragraph
                            .runs
                            .push((std::mem::take(&mut paragraph.line_buf), run_bold));
                    }
                } else if tag.as_ref() == b"w:p" && !in_table {
                    in_paragraph = false;
                    out_lines.push(paragraph_runs_to_markdown_line(&paragraph));
                } else if tag.as_ref() == b"w:tc" && in_table {
                    in_table_cell = false;
                    table_row_buf.push(table_cell_buf.trim().to_string());
                } else if tag.as_ref() == b"w:tr" && in_table {
                    table_rows.push(table_row_buf.clone());
                } else if tag.as_ref() == b"w:tbl" {
                    in_table = false;
                    if !table_rows.is_empty() {
                        let mut table_md = String::new();
                        if let Some(header) = table_rows.first() {
                            table_md.push('|');
                            for cell in header {
                                table_md.push(' ');
                                table_md.push_str(&escape_markdown_table_cell(cell));
                                table_md.push(' ');
                                table_md.push('|');
                            }
                            table_md.push('\n');
                            table_md.push('|');
                            for _ in header {
                                table_md.push_str(" --- | ");
                            }
                            table_md.push('\n');
                        }
                        for row in table_rows.iter().skip(1) {
                            table_md.push('|');
                            for cell in row {
                                table_md.push(' ');
                                table_md.push_str(&escape_markdown_table_cell(cell));
                                table_md.push(' ');
                                table_md.push('|');
                            }
                            table_md.push('\n');
                        }
                        out_lines.push(table_md.trim_end().to_string());
                    }
                    table_rows.clear();
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                return Err(anyhow!("xml parse failed: {e}"));
            }
        }

        buf.clear();
    }

    Ok(out_lines.join("\n"))
}

fn heading_level_from_style(style: &str) -> Option<usize> {
    let normalized = style.to_ascii_lowercase().replace(' ', "");
    if normalized.starts_with("heading1") {
        Some(1)
    } else if normalized.starts_with("heading2") {
        Some(2)
    } else {
        None
    }
}

fn paragraph_runs_to_markdown_line(paragraph: &DocxParagraphAcc) -> String {
    let mut body = String::new();
    for (text, bold) in &paragraph.runs {
        if text.is_empty() {
            continue;
        }

        if *bold {
            body.push_str("**");
            body.push_str(text);
            body.push_str("**");
        } else {
            body.push_str(text);
        }
    }

    let body = body.trim_end().to_string();
    if let Some(level) = paragraph.heading_level {
        let hashes = "#".repeat(level.clamp(1, 6));
        if body.is_empty() {
            format!("{}", hashes)
        } else {
            format!("{} {}", hashes, body)
        }
    } else {
        body
    }
}

pub(crate) fn convert_docx_to_plain_text(src: &Path, _tgt: &Path) -> Result<()> {
    bail!(
        "docx->txt is not wired yet. source exists: {}",
        src.as_os_str().to_string_lossy()
    )
}
