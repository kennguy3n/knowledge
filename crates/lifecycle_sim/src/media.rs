//! Real sample media files for binary content ingestion.
//!
//! Small, deterministic binary fixtures are generated at runtime
//! rather than committed to the repo. Each fixture is a valid file
//! in its respective format with minimal content.

use serde::{Deserialize, Serialize};

/// A media file with bytes and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaFile {
    /// Filename.
    pub filename: String,
    /// MIME type.
    pub mime_type: String,
    /// File bytes.
    pub bytes: Vec<u8>,
    /// Human-readable description.
    pub description: String,
}

/// Generate all media fixtures. Returns a map keyed by media hint
/// (e.g. "png", "wav", "pdf", "mp4", "csv").
pub fn load_media() -> Vec<MediaFile> {
    vec![
        MediaFile {
            filename: "tiny.png".to_string(),
            mime_type: "image/png".to_string(),
            bytes: minimal_png(),
            description: "1×1 pixel transparent PNG".to_string(),
        },
        MediaFile {
            filename: "short.wav".to_string(),
            mime_type: "audio/wav".to_string(),
            bytes: minimal_wav(),
            description: "1 second 440Hz sine wave, 16-bit mono 8kHz".to_string(),
        },
        MediaFile {
            filename: "mini.pdf".to_string(),
            mime_type: "application/pdf".to_string(),
            bytes: minimal_pdf(),
            description: "1-page PDF with 'Test Document' text".to_string(),
        },
        MediaFile {
            filename: "small.mp4".to_string(),
            mime_type: "video/mp4".to_string(),
            bytes: minimal_mp4(),
            description: "Minimal MP4 container".to_string(),
        },
        MediaFile {
            filename: "sample.csv".to_string(),
            mime_type: "text/csv".to_string(),
            bytes: sample_csv(),
            description: "50 rows of structured data".to_string(),
        },
        MediaFile {
            filename: "sample.json".to_string(),
            mime_type: "application/json".to_string(),
            bytes: sample_json(),
            description: "Nested JSON object".to_string(),
        },
        MediaFile {
            filename: "mini.docx".to_string(),
            mime_type: "application/vnd.openxmlformats-officedocument.wordprocessingml.document".to_string(),
            bytes: minimal_docx(),
            description: "Minimal DOCX with 'Test Document' text".to_string(),
        },
    ]
}

/// Pick a media file by hint name.
pub fn media_for_hint<'a>(files: &'a [MediaFile], hint: &str) -> Option<&'a MediaFile> {
    files.iter().find(|f| {
        f.filename
            .ends_with(hint) || f.mime_type.contains(hint)
    })
}

// ── Minimal file generators ──────────────────────────────────────────

/// Minimal valid 1×1 transparent PNG (67 bytes).
fn minimal_png() -> Vec<u8> {
    // PNG signature + IHDR + IDAT + IEND for a 1×1 RGBA transparent pixel.
    let raw: [u8; 67] = [
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // PNG signature
        0x00, 0x00, 0x00, 0x0D, // IHDR length
        0x49, 0x48, 0x44, 0x52, // "IHDR"
        0x00, 0x00, 0x00, 0x01, // width = 1
        0x00, 0x00, 0x00, 0x01, // height = 1
        0x08, 0x06, 0x00, 0x00, 0x00, // bit depth 8, color type 6 (RGBA)
        0x1F, 0x15, 0xC4, 0x89, // CRC
        0x00, 0x00, 0x00, 0x0A, // IDAT length
        0x49, 0x44, 0x41, 0x54, // "IDAT"
        0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, // zlib data
        0x0D, 0x0A, 0x2D, 0xB4, // CRC
        0x00, 0x00, 0x00, 0x00, // IEND length
        0x49, 0x45, 0x4E, 0x44, // "IEND"
        0xAE, 0x42, 0x60, 0x82, // CRC
    ];
    raw.to_vec()
}

/// Minimal WAV file: 1 second of silence, 16-bit mono, 8000 Hz.
fn minimal_wav() -> Vec<u8> {
    let sample_rate: u32 = 8000;
    let num_samples: u32 = sample_rate; // 1 second
    let bits_per_sample: u16 = 16;
    let num_channels: u16 = 1;
    let byte_rate: u32 = sample_rate * u32::from(num_channels) * u32::from(bits_per_sample) / 8;
    let block_align: u16 = num_channels * (bits_per_sample / 8);
    let data_size: u32 = num_samples * u32::from(block_align);
    let chunk_size: u32 = 36 + data_size;

    let mut buf = Vec::with_capacity(44 + data_size as usize);
    // RIFF header
    buf.extend_from_slice(b"RIFF");
    buf.extend_from_slice(&chunk_size.to_le_bytes());
    buf.extend_from_slice(b"WAVE");
    // fmt chunk
    buf.extend_from_slice(b"fmt ");
    buf.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt size
    buf.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    buf.extend_from_slice(&num_channels.to_le_bytes());
    buf.extend_from_slice(&sample_rate.to_le_bytes());
    buf.extend_from_slice(&byte_rate.to_le_bytes());
    buf.extend_from_slice(&block_align.to_le_bytes());
    buf.extend_from_slice(&bits_per_sample.to_le_bytes());
    // data chunk
    buf.extend_from_slice(b"data");
    buf.extend_from_slice(&data_size.to_le_bytes());
    // Silence (zeros)
    buf.resize(44 + data_size as usize, 0);
    buf
}

/// Minimal PDF with one page and "Test Document" text.
fn minimal_pdf() -> Vec<u8> {
    let pdf = "%PDF-1.4\n\
1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n\
2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n\
3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>\nendobj\n\
4 0 obj\n<< /Length 44 >>\nstream\nBT /F1 12 Tf 100 700 Td (Test Document) Tj ET\nendstream\nendobj\n\
5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n\
xref\n0 6\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n0000000115 00000 n \n0000000266 00000 n \n0000000360 00000 n \n\
trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n429\n%%EOF\n";
    pdf.as_bytes().to_vec()
}

/// Minimal MP4 container (just ftyp box — enough to be recognized as MP4).
fn minimal_mp4() -> Vec<u8> {
    let mut buf = Vec::new();
    // ftyp box
    let ftyp_data = b"isom\x00\x00\x02\x00isomiso2mp41";
    let ftyp_size: u32 = 8 + ftyp_data.len() as u32;
    buf.extend_from_slice(&ftyp_size.to_be_bytes());
    buf.extend_from_slice(b"ftyp");
    buf.extend_from_slice(ftyp_data);
    // moov box (empty — minimal valid container)
    let moov_size: u32 = 8;
    buf.extend_from_slice(&moov_size.to_be_bytes());
    buf.extend_from_slice(b"moov");
    buf
}

/// Sample CSV with 50 rows.
fn sample_csv() -> Vec<u8> {
    let mut csv = String::from("id,name,department,salary,start_date\n");
    for i in 1..=50 {
        csv.push_str(&format!(
            "{i},Employee_{i},Dept_{}\t,{},2024-01-{i:02}\n",
            i % 5,
            50000 + i * 1000
        ));
    }
    csv.into_bytes()
}

/// Sample nested JSON.
fn sample_json() -> Vec<u8> {
    let json = serde_json::json!({
        "project": "Knowledge Substrate",
        "version": "1.2.0",
        "modules": ["evidence_store", "observation_engine", "memory_manager"],
        "metrics": {
            "total_evidence": 100000,
            "total_scopes": 200,
            "languages": 22
        },
        "metadata": {
            "created_by": "lifecycle_sim",
            "timestamp": "2024-01-01T00:00:00Z"
        }
    });
    serde_json::to_vec_pretty(&json).unwrap()
}

/// Minimal DOCX file (Office Open XML). A DOCX is a ZIP archive containing
/// XML parts. We generate a minimal valid ZIP with the required parts:
/// `[Content_Types].xml`, `_rels/.rels`, `word/document.xml`.
fn minimal_docx() -> Vec<u8> {
    // DOCX is a ZIP archive. We build a minimal ZIP with 3 entries.
    let content_types = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#;

    let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#;

    let document = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
<w:body><w:p><w:r><w:t>Test Document</w:t></w:r></w:p></w:body>
</w:document>"#;

    // Build a minimal ZIP archive (no compression, stored method).
    let entries: &[(&str, &[u8])] = &[
        ("[Content_Types].xml", content_types.as_bytes()),
        ("_rels/.rels", rels.as_bytes()),
        ("word/document.xml", document.as_bytes()),
    ];

    build_minimal_zip(entries)
}

/// Build a minimal ZIP archive containing the given entries (stored, no compression).
fn build_minimal_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut central_dir: Vec<u8> = Vec::new();
    let mut offset: u32 = 0;

    for (name, data) in entries {
        let name_bytes = name.as_bytes();
        let crc = crc32(data);

        // Local file header
        buf.extend_from_slice(&0x04034b50u32.to_le_bytes()); // signature
        buf.extend_from_slice(&20u16.to_le_bytes()); // version needed
        buf.extend_from_slice(&0u16.to_le_bytes()); // flags
        buf.extend_from_slice(&0u16.to_le_bytes()); // method = stored
        buf.extend_from_slice(&0u16.to_le_bytes()); // mod time
        buf.extend_from_slice(&0u16.to_le_bytes()); // mod date
        buf.extend_from_slice(&crc.to_le_bytes());
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes()); // compressed size
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncompressed size
        buf.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes()); // extra field length
        buf.extend_from_slice(name_bytes);
        buf.extend_from_slice(data);

        // Central directory entry
        central_dir.extend_from_slice(&0x02014b50u32.to_le_bytes()); // signature
        central_dir.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central_dir.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // flags
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // method
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // mod date
        central_dir.extend_from_slice(&crc.to_le_bytes());
        central_dir.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central_dir.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central_dir.extend_from_slice(&(name_bytes.len() as u16).to_le_bytes());
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // extra
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // comment
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // disk number
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central_dir.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central_dir.extend_from_slice(&offset.to_le_bytes()); // local header offset
        central_dir.extend_from_slice(name_bytes);

        offset += 30 + name_bytes.len() as u32 + data.len() as u32;
    }

    let cd_offset = buf.len() as u32;
    let cd_size = central_dir.len() as u32;
    buf.extend_from_slice(&central_dir);

    // End of central directory
    buf.extend_from_slice(&0x06054b50u32.to_le_bytes()); // signature
    buf.extend_from_slice(&0u16.to_le_bytes()); // disk number
    buf.extend_from_slice(&0u16.to_le_bytes()); // disk with CD
    buf.extend_from_slice(&(entries.len() as u16).to_le_bytes()); // entries on disk
    buf.extend_from_slice(&(entries.len() as u16).to_le_bytes()); // total entries
    buf.extend_from_slice(&cd_size.to_le_bytes());
    buf.extend_from_slice(&cd_offset.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes()); // comment length

    buf
}

/// Simple CRC32 (IEEE 802.3 polynomial) for ZIP checksums.
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// Pick a random media file appropriate for the given scenario domain.
/// Uses the provided RNG for deterministic selection.
pub fn random_media_for_scenario(
    files: &[MediaFile],
    scenario_domain: &str,
    rng: &mut rand::rngs::StdRng,
) -> Option<MediaFile> {
    use rand::RngExt;

    // Domain-specific media preferences.
    let preferred: &[&str] = match scenario_domain {
        "product" => &["png", "pdf"],
        "operations" | "engineering" => &["png", "pdf", "csv"],
        "procurement" | "finance" => &["pdf", "csv"],
        "sales" => &["pdf", "wav", "csv"],
        "hr" => &["pdf", "docx"],
        "support" => &["png", "mp4", "csv"],
        "marketing" => &["png", "mp4", "csv"],
        _ => &["pdf", "png", "csv", "wav", "mp4", "json", "docx"],
    };

    let hint = preferred[rng.random_range(0..preferred.len())];
    media_for_hint(files, hint).cloned()
}
