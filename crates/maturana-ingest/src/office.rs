//! PPTX/DOCX text extraction. Both are ZIP archives of XML; slide/paragraph
//! text lives in `<a:t>` / `<w:t>` elements (local name `t`), with paragraphs
//! delimited by `<a:p>` / `<w:p>` (local name `p`). Namespace prefixes differ
//! but local names match, so one extractor serves both formats.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use zip::ZipArchive;

pub fn extract_docx(path: &Path) -> Result<String> {
    let mut archive = open_zip(path)?;
    let xml = read_entry(&mut archive, "word/document.xml")
        .context("DOCX is missing word/document.xml")?;
    Ok(extract_office_text(&xml))
}

pub fn extract_pptx(path: &Path) -> Result<String> {
    let mut archive = open_zip(path)?;
    // Slides in presentation order: ppt/slides/slide1.xml, slide2.xml, ...
    let mut slides: Vec<String> = archive
        .file_names()
        .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
        .map(|s| s.to_string())
        .collect();
    slides.sort_by_key(|n| slide_number(n));

    let mut out = String::new();
    for name in slides {
        if let Ok(xml) = read_entry(&mut archive, &name) {
            out.push_str(&extract_office_text(&xml));
            out.push_str("\n\n");
        }
    }
    Ok(out)
}

fn open_zip(path: &Path) -> Result<ZipArchive<File>> {
    let file = File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    ZipArchive::new(file).with_context(|| format!("{} is not a valid zip archive", path.display()))
}

fn read_entry(archive: &mut ZipArchive<File>, name: &str) -> Result<String> {
    let mut entry = archive
        .by_name(name)
        .with_context(|| format!("zip entry {name} not found"))?;
    let mut buf = String::new();
    entry.read_to_string(&mut buf)?;
    Ok(buf)
}

fn slide_number(name: &str) -> u32 {
    name.trim_start_matches("ppt/slides/slide")
        .trim_end_matches(".xml")
        .parse()
        .unwrap_or(u32::MAX)
}

/// Collect text inside `t` elements; insert a space after each text run and a
/// newline after each paragraph (`p`).
fn extract_office_text(xml: &str) -> String {
    let mut reader = Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == b"t" {
                    in_text = true;
                }
            }
            Ok(Event::Text(t)) => {
                if in_text {
                    if let Ok(s) = t.unescape() {
                        out.push_str(&s);
                    }
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"t" => {
                    in_text = false;
                    out.push(' ');
                }
                b"p" => out.push('\n'),
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::extract_office_text;

    #[test]
    fn extracts_text_runs_and_paragraphs() {
        // Mimics DOCX/PPTX shape with namespaced t/p elements.
        let xml = r#"<w:document><w:body>
            <w:p><w:r><w:t>Hello</w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>
            <w:p><w:r><w:t>Second line</w:t></w:r></w:p>
        </w:body></w:document>"#;
        let text = extract_office_text(xml);
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(text.contains("Second line"));
        // Paragraph break present between the two paragraphs.
        assert!(text.contains('\n'));
    }
}
