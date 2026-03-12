use aho_corasick::AhoCorasick;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use regex::Regex;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

mod web;

#[derive(Parser)]
#[command(name = "sdif", about = "Swiss Drug Interaction Finder")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Build the interactions database from the AmiKo source DB
    Build {
        /// Download the AmiKo source database before building
        #[arg(long)]
        download: bool,
        /// Publish interactions.db to pillbox.oddb.org after building
        #[arg(long)]
        publish: bool,
    },
    /// Check interactions between drugs in a basket
    Check {
        /// Brand names or substance names of drugs to check (e.g. Ponstan Marcoumar Aspirin)
        #[arg(required = true)]
        drugs: Vec<String>,
    },
    /// List all class-level interactions across all drug pairs
    ClassInteractions,
    /// Start the web server
    Serve {
        /// Port to listen on
        #[arg(short, long, default_value = "3000")]
        port: u16,
        /// Include EPha curated interactions alongside Swissmedic FI results
        #[arg(long)]
        epha: bool,
    },
    /// Search interactions by clinical term (e.g. Prothrombinzeit, QT-Verlängerung, Blutungsrisiko)
    Search {
        /// Search term to find in interaction descriptions
        #[arg(required = true)]
        term: String,
        /// Maximum number of results to show (default: all)
        #[arg(short, long)]
        limit: Option<usize>,
    },
}

#[derive(Debug, Clone)]
struct Drug {
    id: i64,
    title: String,
    atc_code: String,
    atc_class: String,
    active_substances: Vec<String>,
    interactions_text: String,
}

#[derive(Debug, Clone)]
struct Interaction {
    drug_title: String,
    drug_substance: String,
    interacting_substance: String,
    description: String,
}

#[derive(Debug, Clone)]
struct EphaInteraction {
    atc1: String,
    atc2: String,
    risk_class: String,
    risk_label: String,
    effect: String,
    mechanism: String,
    measures: String,
    title: String,
    severity_score: u8,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let db_path = "db/amiko_db_full_idx_de.db";
    let output_path = "db/interactions.db";

    match cli.command {
        Some(Commands::Check { drugs }) => {
            let drug_refs: Vec<&str> = drugs.iter().map(|s| s.as_str()).collect();
            basket_check(output_path, &drug_refs)?;
        }
        Some(Commands::ClassInteractions) => {
            list_class_interactions(output_path)?;
        }
        Some(Commands::Serve { port, epha }) => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(web::serve(output_path, port, epha))?;
        }
        Some(Commands::Search { term, limit }) => {
            search_interactions(output_path, &term, limit)?;
        }
        Some(Commands::Build { download, publish }) => {
            if download {
                download_source_db(db_path)?;
                download_atc_csv()?;
                download_epha_csv()?;
            }
            run_build(db_path, output_path)?;
            if publish {
                publish_db(output_path)?;
            }
        }
        None => {
            run_build(db_path, output_path)?;
        }
    }

    Ok(())
}

fn download_source_db(db_path: &str) -> Result<()> {
    let db_dir = std::path::Path::new(db_path).parent().unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(db_dir)?;

    let zip_path = db_dir.join("amiko_db_full_idx_de.zip");
    let url = "http://pillbox.oddb.org/amiko_db_full_idx_de.zip";

    println!("Downloading {}...", url);
    let status = std::process::Command::new("curl")
        .args(&["-L", "-o", zip_path.to_str().unwrap(), url])
        .status()
        .with_context(|| "Failed to run curl")?;
    if !status.success() {
        anyhow::bail!("Download failed");
    }

    println!("Extracting to {}...", db_dir.display());
    let status = std::process::Command::new("unzip")
        .args(&["-o", zip_path.to_str().unwrap(), "-d", db_dir.to_str().unwrap()])
        .status()
        .with_context(|| "Failed to run unzip")?;
    if !status.success() {
        anyhow::bail!("Extraction failed");
    }

    println!("Source database ready.");
    Ok(())
}

fn download_atc_csv() -> Result<()> {
    let csv_dir = "csv";
    std::fs::create_dir_all(csv_dir)?;

    let csv_path = format!("{}/atc.csv", csv_dir);
    let url = "http://pillbox.oddb.org/atc.csv";

    println!("Downloading {}...", url);
    let status = std::process::Command::new("curl")
        .args(&["-L", "-o", &csv_path, url])
        .status()
        .with_context(|| "Failed to run curl")?;
    if !status.success() {
        anyhow::bail!("ATC CSV download failed");
    }

    println!("ATC CSV ready at {}", csv_path);
    Ok(())
}

fn download_epha_csv() -> Result<()> {
    let csv_dir = "csv";
    std::fs::create_dir_all(csv_dir)?;

    let zip_path = format!("{}/drug_interactions_csv_de.zip", csv_dir);
    let url = "http://pillbox.oddb.org/drug_interactions_csv_de.zip";

    println!("Downloading {}...", url);
    let status = std::process::Command::new("curl")
        .args(&["-L", "-o", &zip_path, url])
        .status()
        .with_context(|| "Failed to run curl")?;
    if !status.success() {
        anyhow::bail!("EPha CSV download failed");
    }

    println!("Extracting EPha CSV...");
    let status = std::process::Command::new("unzip")
        .args(&["-o", &zip_path, "-d", csv_dir])
        .status()
        .with_context(|| "Failed to run unzip")?;
    if !status.success() {
        anyhow::bail!("EPha CSV extraction failed");
    }

    println!("EPha CSV ready at csv/drug_interactions_csv_de.csv");
    Ok(())
}

fn publish_db(output_path: &str) -> Result<()> {
    let dest = "zdavatz@65.109.137.20:/var/www/pillbox.oddb.org/";
    println!("Publishing {} to {}...", output_path, dest);
    let status = std::process::Command::new("scp")
        .args(&[output_path, dest])
        .status()
        .with_context(|| "Failed to run scp")?;
    if !status.success() {
        anyhow::bail!("Publish failed (scp returned non-zero)");
    }
    println!("Published successfully.");
    Ok(())
}

fn parse_epha_csv(path: &str) -> Result<Vec<EphaInteraction>> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            println!("EPha CSV not found at {} ({}), skipping EPha integration", path, e);
            return Ok(Vec::new());
        }
    };

    let tag_re = Regex::new(r"<[^>]+>").unwrap();
    let mut results = Vec::new();

    for line in content.lines() {
        let parts: Vec<&str> = line.splitn(3, "||").collect();
        if parts.len() < 3 {
            continue;
        }
        let atc1 = parts[0].trim().to_string();
        let atc2 = parts[1].trim().to_string();
        let html = parts[2];

        // Extract fields from HTML using simple pattern matching
        let extract = |label: &str| -> String {
            let search = format!("{}:</i>", label);
            if let Some(start) = html.find(&search) {
                let after = &html[start + search.len()..];
                let end = after.find("</p>").unwrap_or(after.len());
                let raw = &after[..end];
                let text = tag_re.replace_all(raw, "");
                let text = text.replace("&rarr;", "→")
                    .replace("&amp;", "&")
                    .replace("&lt;", "<")
                    .replace("&gt;", ">")
                    .replace("&nbsp;", " ")
                    .replace("&auml;", "ä")
                    .replace("&ouml;", "ö")
                    .replace("&uuml;", "ü");
                text.trim().to_string()
            } else {
                String::new()
            }
        };

        // Extract title from absTitle div
        let title = if let Some(start) = html.find("class=\"absTitle\">") {
            let after = &html[start + 17..];
            let end = after.find("</div>").unwrap_or(after.len());
            let raw = &after[..end];
            let text = tag_re.replace_all(raw, "");
            text.replace("&rarr;", "→").trim().to_string()
        } else {
            format!("{} → {}", atc1, atc2)
        };

        let risk_label = extract("Risikoklasse");
        let effect = extract("Möglicher Effekt");
        let mechanism = extract("Mechanismus");
        let measures = extract("Empfohlene Massnahmen");

        // Extract risk class letter from label like "Kombination vermeiden (D)"
        let risk_class = risk_label
            .rfind('(')
            .and_then(|p| {
                let rest = &risk_label[p + 1..];
                rest.find(')').map(|e| rest[..e].trim().to_string())
            })
            .unwrap_or_default();

        let severity_score = match risk_class.as_str() {
            "X" => 3,
            "D" => 2,
            "C" => 1,
            "B" => 1,
            "A" => 0,
            _ => 0,
        };

        results.push(EphaInteraction {
            atc1,
            atc2,
            risk_class,
            risk_label,
            effect,
            mechanism,
            measures,
            title,
            severity_score,
        });
    }

    Ok(results)
}

fn load_atc_csv(path: &str) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read ATC CSV: {}", path))?;
    for line in content.lines().skip(1) {
        // Format: ATC code,Name,DDD,U,Adm.R,Note
        // Name may be quoted (contains commas)
        let code_end = match line.find(',') {
            Some(pos) => pos,
            None => continue,
        };
        let code = line[..code_end].trim().to_string();
        if code.is_empty() {
            continue;
        }
        let rest = &line[code_end + 1..];
        // Extract name (may be quoted)
        let name = if rest.starts_with('"') {
            let end = rest[1..].find('"').map(|p| p + 1).unwrap_or(rest.len());
            rest[1..end].to_string()
        } else {
            rest.split(',').next().unwrap_or("").to_string()
        };
        map.insert(code, name);
    }
    Ok(map)
}

fn run_build(db_path: &str, output_path: &str) -> Result<()> {
    println!("=== Swiss Drug Interaction Finder (SDIF) ===");
    println!("Reading source database: {}", db_path);

    let atc_csv_path = "csv/atc.csv";
    let atc_map = if std::path::Path::new(atc_csv_path).exists() {
        let map = load_atc_csv(atc_csv_path)?;
        println!("Loaded {} ATC codes from {}", map.len(), atc_csv_path);
        Some(map)
    } else {
        println!("Warning: {} not found, skipping ATC cross-check (run with --download to fetch it)", atc_csv_path);
        None
    };

    let source = Connection::open(db_path)
        .with_context(|| format!("Failed to open source DB: {}", db_path))?;

    let drugs = parse_all_drugs(&source, atc_map.as_ref())?;
    println!("Parsed {} drugs", drugs.len());

    let substance_to_brands = build_substance_brand_map(&drugs);
    println!(
        "Built substance-to-brand map with {} substances",
        substance_to_brands.len()
    );

    let interactions = extract_interactions(&drugs)?;
    println!("Extracted {} interaction records", interactions.len());

    let epha = parse_epha_csv("csv/drug_interactions_csv_de.csv")?;
    println!("Parsed {} EPha interaction records", epha.len());

    write_interactions_db(output_path, &drugs, &interactions, &substance_to_brands, &epha)?;
    println!("Wrote interactions database to: {}", output_path);

    // Severity stats
    let mut sev_counts = [0u32; 4];
    for interaction in &interactions {
        let (score, _) = score_severity(&interaction.description);
        sev_counts[score as usize] += 1;
    }
    let classified = interactions.len() as u32 - sev_counts[0];
    let pct = if interactions.is_empty() { 0 } else {
        (classified as f64 / interactions.len() as f64 * 100.0) as u32
    };

    let drugs_with_interactions = drugs.iter().filter(|d| !d.interactions_text.is_empty()).count();

    let unique_pairs: HashSet<(&str, &str)> = interactions
        .iter()
        .map(|i| {
            let a = i.drug_substance.as_str();
            let b = i.interacting_substance.as_str();
            if a <= b { (a, b) } else { (b, a) }
        })
        .collect();

    println!("\n--- Build Statistics ---");
    println!("  Drugs total:         {}", drugs.len());
    println!("  With interactions:   {}", drugs_with_interactions);
    println!("  Unique substances:   {}", substance_to_brands.len());
    println!("  Interaction records: {} ({} unique substance pairs)", interactions.len(), unique_pairs.len());
    println!("  Severity breakdown:");
    println!("    ### Kontraindiziert:  {:>6}", sev_counts[3]);
    println!("    ##  Schwerwiegend:    {:>6}", sev_counts[2]);
    println!("    #   Vorsicht:         {:>6}", sev_counts[1]);
    println!("    -   Keine Einstufung: {:>6}", sev_counts[0]);
    println!("  Classified: {}%", pct);

    Ok(())
}

fn strip_html(html: &str) -> String {
    let mut result = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
            result.push(' ');
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            result.push(ch);
        }
    }
    // Decode HTML entities
    let result = result
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ");
    let entity_re = Regex::new(r"&#\d+;").unwrap();
    let result = entity_re.replace_all(&result, " ");
    let ws_re = Regex::new(r"\s+").unwrap();
    ws_re.replace_all(&result, " ").trim().to_string()
}

fn extract_interaction_section(content: &str) -> String {
    let section_start_re = Regex::new(r#"id="section(\d+)">"#).unwrap();
    let mut positions: Vec<(u32, usize)> = section_start_re
        .captures_iter(content)
        .filter_map(|cap| {
            let num: u32 = cap[1].parse().ok()?;
            let end = cap.get(0)?.end();
            Some((num, end))
        })
        .collect();
    positions.sort_by_key(|&(_, pos)| pos);

    let mut main_section = String::new();
    let mut supplementary = String::new();

    for (i, &(_, start)) in positions.iter().enumerate() {
        let end = if i + 1 < positions.len() {
            let next = positions[i + 1].1;
            content[..next].rfind("<div").unwrap_or(next)
        } else {
            content.len()
        };
        let text = strip_html(&content[start..end]);

        // Primary: the dedicated "Interaktionen" chapter
        if text.starts_with("Interaktionen") {
            main_section = text;
            continue;
        }

        // Supplementary: other chapters that contain interaction-relevant content
        // e.g. "Warnhinweise und Vorsichtsmassnahmen", "Kontraindikationen"
        let is_relevant_section = text.starts_with("Warnhinweise")
            || text.starts_with("Kontraindikationen")
            || text.starts_with("Dosierung");

        if !is_relevant_section {
            continue;
        }

        // Extract sentences/paragraphs that mention interactions
        let interaction_keywords = [
            "interaktion", "wechselwirkung", "nicht kombinier",
            "nicht gleichzeitig", "kontraindiziert mit",
            "zusammen mit", "bei gleichzeitiger",
            "potenzier", "neuromuskuläre blockade",
        ];

        for sentence in text.split('.') {
            let sentence_lower = sentence.to_lowercase();
            let mentions_interaction = interaction_keywords
                .iter()
                .any(|kw| sentence_lower.contains(kw));
            if mentions_interaction && sentence.len() > 20 {
                if !supplementary.is_empty() {
                    supplementary.push_str(". ");
                }
                supplementary.push_str(sentence.trim());
            }
        }
    }

    if !supplementary.is_empty() {
        if main_section.is_empty() {
            return supplementary;
        }
        format!("{} [Warnhinweise/Kontraindikationen:] {}", main_section, supplementary)
    } else {
        main_section
    }
}

/// Extract active substance names from the Zusammensetzung/Wirkstoffe section of the HTML content.
/// Falls back to this when the ATC column has a code but no substance name.
fn extract_substances_from_html(content: &str) -> Vec<String> {
    // Match Wirkstoff header — two patterns:
    // 1. Header-only: "Wirkstoffe</p>" (substance in next <p>)
    // 2. Inline: "Wirkstoff: Ceritinib.</p>" or "Wirkstoffe: Mercaptamin (als ...).</p>"
    let wirkstoff_header_re = Regex::new(r"Wirkstoff(?:e|\(e\))?(?:\s|&#\d+;)*</p>").unwrap();
    let wirkstoff_inline_re = Regex::new(
        r"Wirkstoff(?:e|\(e\))?(?:\s*:\s*|\s+)([^<]+)</p>"
    ).unwrap();
    let p_tag_re = Regex::new(r#"<p[^>]*class="spacing1"[^>]*>(.*?)</p>"#).unwrap();
    let italic_re = Regex::new(r#"font-style:\s*italic"#).unwrap();
    let html_entity_re = Regex::new(r"&#\d+;").unwrap();
    let html_tag_re = Regex::new(r"<[^>]+>").unwrap();
    let und_re = Regex::new(r"\s+und\s+|\s+et\s+").unwrap();

    let mut substance_text = String::new();

    // Find the Zusammensetzung section title (in absTitle div), not a mention in body text
    let zusammensetzung_re = Regex::new(
        r#"(?i)class="absTitle"[^>]*>\s*Zusammensetzung"#
    ).unwrap();
    let zus_start = match zusammensetzung_re.find(content) {
        Some(m) => m.start(),
        None => return Vec::new(),
    };
    // Limit to the Zusammensetzung section (until next absTitle div)
    let next_section_re = Regex::new(r#"class="absTitle""#).unwrap();
    let zus_end = next_section_re.find(&content[zus_start + 10..])
        .map(|m| zus_start + 10 + m.start())
        .unwrap_or(content.len());
    let zus_content = &content[zus_start..zus_end];

    // Try inline pattern first: "Wirkstoff: SubstanceName</p>" or "Wirkstoffe: X.</p>"
    if let Some(cap) = wirkstoff_inline_re.captures(zus_content) {
        let text = cap[1].trim().to_string();
        if !text.is_empty() {
            substance_text = text;
        }
    }

    // Otherwise use header-only pattern and collect from following <p> tags
    if substance_text.is_empty() {
        let wirkstoff_pos = match wirkstoff_header_re.find(zus_content) {
            Some(m) => m.end(),
            None => return Vec::new(),
        };
        let absolute_pos = zus_start + wirkstoff_pos;

        let remainder = &content[absolute_pos..];
        for cap in p_tag_re.captures_iter(remainder) {
            let full_tag = cap[0].to_string();
            let text = html_tag_re.replace_all(&cap[1], "").trim().to_string();
            let text_lower = text.to_lowercase();

            // Stop at Hilfsstoff(e)
            if text_lower.contains("hilfsstoff") || text_lower.contains("excipient") {
                break;
            }

            // Skip italic sub-headers (dosage forms like "Morgendosis:", "Durchstechflasche:")
            // but don't break — more substance paragraphs may follow
            if italic_re.is_match(&full_tag) {
                continue;
            }

            if !substance_text.is_empty() {
                substance_text.push_str(", ");
            }
            substance_text.push_str(&text);
        }
    }

    if substance_text.is_empty() {
        return Vec::new();
    }

    // Replace HTML entities (&#32; = space, etc.)
    let substance_text = html_entity_re.replace_all(&substance_text, " ").to_string();

    let mut substances = Vec::new();
    // Split by comma for multi-substance entries
    for part in substance_text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        // Split by "und" / "et" for entries like "Nivolumab und Relatlimab"
        for name_raw in und_re.split(part) {
            let name = extract_substance_name(name_raw.trim());
            if name.len() > 2 && !substances.contains(&name) {
                substances.push(name);
            }
        }
    }

    substances
}

/// Clean up a single substance name from Wirkstoffe text.
/// Handles patterns like:
///   "Trametinib als Trametinibdimethylsulfoxid" → "Trametinib"
///   "Desvenlafaxinum ut desvenlafaxini benzoas" → "Desvenlafaxin"
///   "Fedratinib (als Dihydrochlorid-Monohydrat)." → "Fedratinib"
///   "Bimekizumab, aus gentechnisch..." → "Bimekizumab"
fn extract_substance_name(text: &str) -> String {
    let mut name = text.to_string();

    // Remove trailing period
    name = name.trim_end_matches('.').trim().to_string();

    // Truncate at footnote markers (* or **)
    if let Some(pos) = name.find('*') {
        name = name[..pos].trim().to_string();
    }

    // Take text before "als" or "ut" (salt form indicator)
    for separator in &[" als ", " ut "] {
        if let Some(pos) = name.to_lowercase().find(separator) {
            name = name[..pos].trim().to_string();
        }
    }

    // Remove parenthesized content
    if let Some(pos) = name.find('(') {
        name = name[..pos].trim().to_string();
    }

    // Truncate at descriptive phrases: "ist ein...", "wird ...", "aus ..."
    for phrase in &[" ist ein", " wird ", " aus ", " der ", " die ", " das ", " ein "] {
        if let Some(pos) = name.to_lowercase().find(phrase) {
            // Ensure we slice at valid char boundary in original string
            if name.is_char_boundary(pos) {
                name = name[..pos].trim().to_string();
            }
        }
    }

    // Strip trailing period again
    name = name.trim_end_matches('.').trim().to_string();

    // Strip Latin -um suffix to get INN name (e.g. Desvenlafaxinum → Desvenlafaxin)
    // But only for typical Latin pharmaceutical suffixes, not words like "Aluminium"
    if name.ends_with("um") && name.len() > 6 {
        let stem = &name[..name.len() - 2];
        let lower = name.to_lowercase();
        if !lower.ends_with("ium") && !lower.ends_with("eum") {
            name = stem.to_string();
        }
    }

    // Substance names are at most 3 words (e.g. "Lisocabtagen Maraleucel")
    // Anything longer is likely a description leaking through
    let words: Vec<&str> = name.split_whitespace().collect();
    if words.len() > 3 {
        name = words[..3].join(" ");
    }

    // Substance names must start with an uppercase letter (or digit for things like "18F-...")
    let first_char = name.chars().next().unwrap_or(' ');
    if !first_char.is_uppercase() && !first_char.is_ascii_digit() {
        return String::new();
    }

    // Reject fragments that start with common German words (sentence fragments, not substance names)
    let lower = name.to_lowercase();
    let reject_starts = [
        "jeder ", "die ", "der ", "das ", "den ", "dem ", "des ",
        "ein ", "eine ", "einer ", "einem ", "einen ",
        "enthält", "besteht", "gereinigt", "konjugiert",
    ];
    for prefix in &reject_starts {
        if lower.starts_with(prefix) {
            return String::new();
        }
    }

    name
}

fn parse_all_drugs(conn: &Connection, atc_map: Option<&HashMap<String, String>>) -> Result<Vec<Drug>> {
    let mut stmt = conn.prepare(
        "SELECT _id, title, atc, atc_class, content FROM amikodb WHERE content IS NOT NULL AND content != ''",
    )?;

    let rows: Vec<(i64, String, String, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, String>(4)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    println!("Processing {} drug entries...", rows.len());

    let und_re = Regex::new(r"\s+und\s+|\s+et\s+").unwrap();

    let mut drugs = Vec::new();
    let mut html_fallback_count = 0u32;
    let mut atc_valid_count = 0u32;
    let mut atc_invalid_count = 0u32;
    for (idx, (id, title, atc, atc_class, content)) in rows.iter().enumerate() {
        if idx % 500 == 0 {
            eprint!("\r  Parsing drug {}/{}...", idx, rows.len());
        }

        let mut active_substances: Vec<String> = Vec::new();
        let mut atc_code = String::new();

        if !atc.is_empty() {
            let parts: Vec<&str> = atc.splitn(2, ';').collect();
            if parts.len() == 2 {
                atc_code = parts[0].to_string();
                for name in und_re.split(parts[1]) {
                    let name = name.trim().to_string();
                    if name.len() > 2 {
                        active_substances.push(name);
                    }
                }
            }
        }

        // Cross-check ATC code against official ATC CSV
        if let Some(ref map) = atc_map {
            if !atc_code.is_empty() {
                if map.contains_key(&atc_code) {
                    atc_valid_count += 1;
                } else {
                    atc_invalid_count += 1;
                    eprintln!("  ATC mismatch: {} not found in atc.csv ({})", atc_code, title.trim());
                }
            }
        }

        // Fallback: extract from Zusammensetzung/Wirkstoffe HTML section
        if active_substances.is_empty() && !atc_code.is_empty() {
            active_substances = extract_substances_from_html(content);
            if !active_substances.is_empty() {
                html_fallback_count += 1;
            }
        }

        let interactions_text = extract_interaction_section(content);

        if active_substances.is_empty() {
            continue;
        }

        drugs.push(Drug {
            id: *id,
            title: title.trim().to_string(),
            atc_code,
            atc_class: atc_class.clone(),
            active_substances,
            interactions_text,
        });
    }
    eprintln!("\r  Parsing done.                    ");
    if html_fallback_count > 0 {
        println!("  Extracted substances from HTML for {} drugs (ATC column had no substance name)", html_fallback_count);
    }
    if atc_map.is_some() {
        println!("  ATC cross-check: {} valid, {} invalid",
            atc_valid_count, atc_invalid_count);
    }

    Ok(drugs)
}

/// Derive the administration route from ATC code and brand name.
/// Returns a short label: "topisch", "p.o.", "i.v.", "s.c.", "i.m.", "inhalativ", "nasal", "rektal", "ophthalm.", "otisch", or "".
fn derive_route(atc_code: &str, brand_name: &str) -> &'static str {
    let name = brand_name.to_lowercase();

    // ATC-based topical routes
    if atc_code.starts_with("D") && !atc_code.starts_with("D05BB") && !atc_code.starts_with("D05BX") {
        // D = Dermatika, except D05BB/D05BX = systemische Antipsoriatika
        return "topisch";
    }
    if atc_code.starts_with("C05BA") || atc_code.starts_with("C05BB") {
        return "topisch"; // topische Heparinoide/Vasoprotektoren
    }
    if atc_code.starts_with("S01") {
        return "ophthalm.";
    }
    if atc_code.starts_with("S02") {
        return "otisch";
    }
    if atc_code.starts_with("S03") {
        return "ophthalm."; // Ophthalmologische und otologische
    }
    if atc_code.starts_with("A01A") {
        return "topisch"; // Stomatologika
    }

    // Brand name patterns (more specific first)
    if name.contains("infusion") || name.contains(" i.v.") || name.contains(",i.v.") || name.contains("konzentrat zur herstellung") {
        return "i.v.";
    }
    if name.contains(" s.c.") || name.contains("subkutan") || name.contains("fertigspritze") || name.contains("fertigpen") || name.contains("pen ") {
        return "s.c.";
    }
    if name.contains(" i.m.") || name.contains("intramuskulär") {
        return "i.m.";
    }
    if name.contains(" gel") || name.contains(" creme") || name.contains(" cream") || name.contains(" salbe") || name.contains(" paste") || name.contains(" pflaster") || name.contains(" patch") {
        return "topisch";
    }
    if name.contains("inhalat") || name.contains("dosieraerosol") || name.contains("turbuhaler") || name.contains("diskus") || name.contains("breezhaler") || name.contains("pulverinhalat") {
        return "inhalativ";
    }
    if name.contains("nasenspray") || name.contains("nasenlösung") || name.contains("rhinospray") {
        return "nasal";
    }
    if name.contains("suppositor") || name.contains("rektal") || name.contains("klysma") {
        return "rektal";
    }
    if name.contains("augentropfen") || name.contains("augensalbe") || name.contains("ophtha") {
        return "ophthalm.";
    }
    if name.contains("ohrentropfen") {
        return "otisch";
    }

    // ATC-based nasal/inhalativ
    if atc_code.starts_with("R01") {
        return "nasal";
    }
    if atc_code.starts_with("R03") {
        if name.contains("tabletten") || name.contains("lösung zum einnehmen") || name.contains("sirup") {
            return "p.o.";
        }
        return "inhalativ";
    }

    // Default: most drugs are oral
    ""
}

/// Derive a combination therapy hint from brand name and drug properties.
/// Returns a hint string for known approved combination therapies, or empty.
fn derive_combo_hint(brand_name: &str, atc_code: &str, substances: &[String]) -> String {
    let name_lower = brand_name.to_lowercase();

    // Rivaroxaban vascular = approved for use WITH ASS
    if name_lower.contains("vascular") && atc_code == "B01AF01" {
        return "Zugelassene Kombitherapie mit niedrigdosierter Acetylsalicylsäure (ASS 75–100 mg) zur Prävention atherothrombotischer Ereignisse".to_string();
    }

    // Multi-substance combination products: the contained substances are combined by design
    if substances.len() > 1 {
        return format!("Kombipräparat mit {} Wirkstoffen", substances.len());
    }

    String::new()
}

fn build_substance_brand_map(drugs: &[Drug]) -> HashMap<String, Vec<String>> {
    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for drug in drugs {
        for subst in &drug.active_substances {
            let key = subst.to_lowercase();
            map.entry(key).or_default().push(drug.title.clone());
        }
    }
    map
}

fn extract_interactions(drugs: &[Drug]) -> Result<Vec<Interaction>> {
    let all_substances: Vec<String> = drugs
        .iter()
        .flat_map(|d| d.active_substances.iter())
        .map(|s| s.to_lowercase())
        .collect::<HashSet<_>>()
        .into_iter()
        .filter(|s| s.len() >= 4)
        .collect();

    println!(
        "Building Aho-Corasick automaton for {} substances...",
        all_substances.len()
    );
    let ac = AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build(&all_substances)?;

    let mut interactions = Vec::new();
    let drugs_with_interactions: Vec<&Drug> = drugs
        .iter()
        .filter(|d| !d.interactions_text.is_empty())
        .collect();

    println!(
        "Scanning {} drugs with interaction texts...",
        drugs_with_interactions.len()
    );

    for (idx, drug) in drugs_with_interactions.iter().enumerate() {
        if idx % 500 == 0 {
            eprint!(
                "\r  Scanning drug {}/{}...",
                idx,
                drugs_with_interactions.len()
            );
        }

        let own_substances: HashSet<String> = drug
            .active_substances
            .iter()
            .map(|s| s.to_lowercase())
            .collect();

        let mut found: HashSet<usize> = HashSet::new();
        for mat in ac.find_iter(&drug.interactions_text) {
            found.insert(mat.pattern().as_usize());
        }

        let drug_substance = drug.active_substances.join(", ");

        for pattern_idx in found {
            let substance = &all_substances[pattern_idx];

            if own_substances.contains(substance) {
                continue;
            }

            if is_common_word(substance) {
                continue;
            }

            let description = extract_context(&drug.interactions_text, substance);

            interactions.push(Interaction {
                drug_title: drug.title.clone(),
                drug_substance: drug_substance.clone(),
                interacting_substance: substance.clone(),
                description,
            });
        }
    }
    eprintln!("\r  Scanning done.                    ");

    Ok(interactions)
}

/// Score interaction severity based on German keywords in the description text.
/// Returns (numeric_score, label) where score is 0-3:
///   3 = "Kontraindiziert" — contraindicated, must not combine
///   2 = "Schwerwiegend"   — serious risk, avoid if possible
///   1 = "Vorsicht"        — use with caution, monitor
///   0 = "Keine Einstufung" — no severity keywords found
/// Strip section references and isolated labels that mention contraindication
/// keywords but are not actual contraindication statements.
/// E.g. "siehe «Kontraindikationen»", "(siehe Rubrik Kontraindikationen)",
/// "Kontraindiziert!" as an isolated FI table header/label
fn strip_section_references(text: &str) -> String {
    // First normalize: remove guillemets «» so "siehe «Kontraindikationen»" becomes
    // "siehe Kontraindikationen" for easier pattern matching
    let normalized = text.replace('\u{ab}', "").replace('\u{bb}', ""); // « and »
    let mut result = normalized;
    // Strip isolated "Kontraindiziert!" labels (FI table headers, not clinical statements)
    // These appear as "kontraindiziert! <substance>" at the start of a description
    if result.starts_with("kontraindiziert!") {
        result = result["kontraindiziert!".len()..].trim_start().to_string();
    }
    // Common FI cross-reference patterns (already lowercased input)
    let patterns = [
        "siehe kontraindikation",
        "siehe rubrik kontraindikation",
        "siehe abschnitt kontraindikation",
        "siehe auch kontraindikation",
        "siehe auch rubrik kontraindikation",
        "siehe kapitel kontraindikation",
    ];
    for pat in &patterns {
        // Find and blank out the whole reference including surrounding ()
        while let Some(pos) = result.find(pat) {
            // Expand backwards to include opening (
            let start = if pos > 0 {
                let prev = &result[..pos];
                if prev.ends_with('(') { pos - 1 } else { pos }
            } else { pos };
            // Expand forwards to end of reference (next sentence boundary or closing bracket)
            let rest = &result[pos..];
            let end = rest.find(|c: char| c == '.' || c == ';' || c == ')' || c == '\n')
                .map(|i| pos + i + 1)
                .unwrap_or(result.len());
            result.replace_range(start..end, " ");
        }
    }
    result
}

pub fn score_severity(text: &str) -> (u8, &'static str) {
    let lower = text.to_lowercase();

    // Strip section references before scoring — "siehe «Kontraindikationen»"
    // refers to an FI rubric, not an actual contraindication statement
    let stripped = strip_section_references(&lower);

    // Level 3: Contraindicated
    let contraindicated = [
        "kontraindiziert",
        "kontraindikation",
        "darf nicht",
        "nicht angewendet werden",
        "nicht verabreicht werden",
        "nicht kombiniert werden",
        "nicht gleichzeitig",
        "ist verboten",
        "absolut kontraindiziert",
        "streng kontraindiziert",
        "nicht zusammen",
        "nicht eingenommen werden",
        "nicht anwenden",
    ];
    for kw in &contraindicated {
        if stripped.contains(kw) {
            return (3, "Kontraindiziert");
        }
    }

    // Level 2: Serious / high risk
    let serious = [
        "erhöhtes risiko",
        "erhöhte gefahr",
        "schwerwiegend",
        "schwere",
        "lebensbedrohlich",
        "lebensgefährlich",
        "gefährlich",
        "stark erhöht",
        "stark verstärkt",
        "toxisch",
        "toxizität",
        "nephrotoxisch",
        "hepatotoxisch",
        "ototoxisch",
        "neurotoxisch",
        "kardiotoxisch",
        "tödlich",
        "fatale",
        "blutungsrisiko",
        "blutungsgefahr",
        "serotoninsyndrom",
        "serotonin-syndrom",
        "qt-verlängerung",
        "qt-zeit-verlängerung",
        "torsade",
        "rhabdomyolyse",
        "nierenversagen",
        "niereninsuffizienz",
        "nierenfunktionsstörung",
        "leberversagen",
        "atemdepression",
        "herzstillstand",
        "arrhythmie",
        "hyperkaliämie",
        "agranulozytos",
        "stevens-johnson",
        "anaphyla",
        "lymphoproliferation",
        "immundepression",
        "immunsuppression",
        "panzytopenie",
        "abgeraten",
        "wird nicht empfohlen",
    ];
    for kw in &serious {
        if lower.contains(kw) {
            return (2, "Schwerwiegend");
        }
    }

    // Level 1: Caution / monitor
    let caution = [
        "vorsicht",
        "überwach",
        "monitor",
        "kontroll",
        "engmaschig",
        "dosisanpassung",
        "dosis reduz",
        "dosis anpassen",
        "dosisreduktion",
        "sorgfältig",
        "regelmässig",
        "regelmäßig",
        "aufmerksam",
        "cave",
        "beobacht",
        "verstärkt",
        "vermindert",
        "abgeschwächt",
        "erhöh",
        "erniedrigt",
        "beeinflusst",
        "wechselwirkung",
        "plasmaspiegel",
        "plasmakonzentration",
        "serumkonzentration",
        "bioverfügbarkeit",
        "subtherapeutisch",
        "supratherapeutisch",
        "therapieversagen",
        "wirkungsverlust",
        "wirkverlust",
    ];
    for kw in &caution {
        if lower.contains(kw) {
            return (1, "Vorsicht");
        }
    }

    (0, "Keine Einstufung")
}

pub fn severity_indicator(score: u8) -> &'static str {
    match score {
        3 => "###",
        2 => "##",
        1 => "#",
        _ => "-",
    }
}

fn is_common_word(s: &str) -> bool {
    matches!(
        s,
        "oder" | "anti" | "wasser" | "wirkstoffe" | "nicht" | "aber" | "auch"
            | "wird" | "kann" | "sind" | "eine" | "dies" | "nach" | "über"
            | "mehr" | "alle" | "dazu" | "etwa" | "noch" | "hier" | "sehr"
            | "gabe" | "glas" | "darm" | "laut" | "teil" | "fall" | "form"
    )
}

/// Extract the most clinically relevant context snippet for a substance mention.
/// Scans all occurrences in the text and returns the one with the highest severity score.
fn extract_context(text: &str, substance: &str) -> String {
    let lower = text.to_lowercase();
    let mut best_snippet = String::new();
    let mut best_severity: u8 = 0;
    let mut best_is_animal = false;
    let mut search_from = 0;

    while let Some(rel_pos) = lower[search_from..].find(substance) {
        let pos = search_from + rel_pos;
        let start = lower[..pos]
            .rfind(|c: char| c == '.' || c == ':')
            .map(|p| p + 1)
            .unwrap_or(0);
        let end = lower[pos..]
            .find('.')
            .map(|p| pos + p + 1)
            .unwrap_or(text.len());

        let snippet = text[start..end.min(text.len())].trim();
        let (sev, _) = score_severity(snippet);

        // Deprioritize snippets where the substance appears after "Tiermodell" —
        // this indicates the substance is mentioned incidentally in an animal study
        // reference for a different interaction partner (e.g. Verapamil+Dantrolen
        // animal model cited in Amlodipin FI, falsely attributed to Amlodipin↔Verapamil)
        let prefix_lower = &lower[start..pos];
        let is_animal_model = prefix_lower.contains("tiermodell")
            || prefix_lower.contains("tierstudie")
            || prefix_lower.contains("tierversuch");
        let effective_sev = if is_animal_model { 0 } else { sev };

        let dominated = effective_sev > best_severity
            || (effective_sev == best_severity && best_is_animal && !is_animal_model)
            || best_snippet.is_empty();

        if dominated {
            best_severity = effective_sev;
            best_is_animal = is_animal_model;
            best_snippet = if snippet.len() > 500 {
                let mut trunc = 497;
                while !snippet.is_char_boundary(trunc) && trunc > 0 {
                    trunc -= 1;
                }
                format!("{}...", &snippet[..trunc])
            } else {
                snippet.to_string()
            };
            // Can't do better than Kontraindiziert
            if best_severity >= 3 {
                break;
            }
        }

        search_from = pos + substance.len();
    }

    best_snippet
}

fn write_interactions_db(
    path: &str,
    drugs: &[Drug],
    interactions: &[Interaction],
    substance_to_brands: &HashMap<String, Vec<String>>,
    epha_interactions: &[EphaInteraction],
) -> Result<()> {
    let _ = std::fs::remove_file(path);
    let conn = Connection::open(path)?;

    conn.execute_batch(
        "
        CREATE TABLE drugs (
            id INTEGER PRIMARY KEY,
            brand_name TEXT NOT NULL,
            atc_code TEXT,
            atc_class TEXT,
            active_substances TEXT NOT NULL,
            interactions_text TEXT,
            route TEXT NOT NULL DEFAULT '',
            combo_hint TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX idx_drugs_brand ON drugs(brand_name);
        CREATE INDEX idx_drugs_atc ON drugs(atc_code);

        CREATE TABLE interactions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            drug_brand TEXT NOT NULL,
            drug_substance TEXT NOT NULL,
            interacting_substance TEXT NOT NULL,
            interacting_brands TEXT,
            description TEXT NOT NULL,
            severity_score INTEGER NOT NULL DEFAULT 0,
            severity_label TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX idx_interactions_brand ON interactions(drug_brand);
        CREATE INDEX idx_interactions_substance ON interactions(interacting_substance);

        CREATE TABLE substance_brand_map (
            substance TEXT NOT NULL,
            brand_name TEXT NOT NULL,
            route TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX idx_sbm_substance ON substance_brand_map(substance);
        CREATE INDEX idx_sbm_brand ON substance_brand_map(brand_name);

        CREATE TABLE epha_interactions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            atc1 TEXT NOT NULL,
            atc2 TEXT NOT NULL,
            risk_class TEXT NOT NULL DEFAULT '',
            risk_label TEXT NOT NULL DEFAULT '',
            effect TEXT NOT NULL DEFAULT '',
            mechanism TEXT NOT NULL DEFAULT '',
            measures TEXT NOT NULL DEFAULT '',
            title TEXT NOT NULL DEFAULT '',
            severity_score INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX idx_epha_atc1 ON epha_interactions(atc1);
        CREATE INDEX idx_epha_atc2 ON epha_interactions(atc2);
        CREATE INDEX idx_epha_pair ON epha_interactions(atc1, atc2);

        CREATE TABLE class_keywords (
            atc_prefix TEXT NOT NULL,
            keyword TEXT NOT NULL
        );
        CREATE INDEX idx_class_kw_prefix ON class_keywords(atc_prefix);

        CREATE TABLE cyp_rules (
            enzyme TEXT NOT NULL,
            text_pattern TEXT NOT NULL,
            role TEXT NOT NULL,
            atc_prefix TEXT,
            substance TEXT
        );
        CREATE INDEX idx_cyp_enzyme ON cyp_rules(enzyme);
        CREATE INDEX idx_cyp_role ON cyp_rules(role);
        ",
    )?;

    conn.execute_batch("BEGIN TRANSACTION")?;

    {
        let mut stmt = conn.prepare(
            "INSERT INTO drugs (id, brand_name, atc_code, atc_class, active_substances, interactions_text, route, combo_hint) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        for drug in drugs {
            let route = derive_route(&drug.atc_code, &drug.title);
            let combo_hint = derive_combo_hint(&drug.title, &drug.atc_code, &drug.active_substances);
            stmt.execute(params![
                drug.id,
                drug.title,
                drug.atc_code,
                drug.atc_class,
                drug.active_substances.join(", "),
                drug.interactions_text,
                route,
                combo_hint,
            ])?;
        }
    }

    {
        let mut stmt = conn.prepare(
            "INSERT INTO interactions (drug_brand, drug_substance, interacting_substance, interacting_brands, description, severity_score, severity_label) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for interaction in interactions {
            let interacting_brands = substance_to_brands
                .get(&interaction.interacting_substance)
                .map(|brands| brands.join(", "))
                .unwrap_or_default();

            let (sev_score, sev_label) = score_severity(&interaction.description);

            stmt.execute(params![
                interaction.drug_title,
                interaction.drug_substance,
                interaction.interacting_substance,
                interacting_brands,
                interaction.description,
                sev_score,
                sev_label,
            ])?;
        }
    }

    {
        let mut stmt = conn
            .prepare("INSERT INTO substance_brand_map (substance, brand_name, route) VALUES (?1, ?2, ?3)")?;
        let brand_route: HashMap<&str, &str> = drugs.iter()
            .map(|d| (d.title.as_str(), derive_route(&d.atc_code, &d.title)))
            .collect();
        for (substance, brands) in substance_to_brands {
            for brand in brands {
                let route = brand_route.get(brand.as_str()).copied().unwrap_or("");
                stmt.execute(params![substance, brand, route])?;
            }
        }
    }

    {
        let mut stmt = conn.prepare(
            "INSERT INTO epha_interactions (atc1, atc2, risk_class, risk_label, effect, mechanism, measures, title, severity_score) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )?;
        for epha in epha_interactions {
            stmt.execute(params![
                epha.atc1,
                epha.atc2,
                epha.risk_class,
                epha.risk_label,
                epha.effect,
                epha.mechanism,
                epha.measures,
                epha.title,
                epha.severity_score,
            ])?;
        }
    }

    // Insert class keywords from keywords.txt
    {
        let mut stmt = conn.prepare(
            "INSERT INTO class_keywords (atc_prefix, keyword) VALUES (?1, ?2)",
        )?;
        for (prefix, keywords) in parse_class_keywords() {
            for keyword in &keywords {
                stmt.execute(params![prefix, keyword])?;
            }
        }
    }

    // Insert CYP rules
    {
        let mut stmt = conn.prepare(
            "INSERT INTO cyp_rules (enzyme, text_pattern, role, atc_prefix, substance) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for (enzyme, text_patterns, inhib_atc, inhib_subst, induc_atc, induc_subst) in cyp_rule_data() {
            for pattern in &text_patterns {
                // Inhibitor ATC prefixes
                for atc in &inhib_atc {
                    stmt.execute(params![enzyme, pattern, "inhibitor", atc, Option::<&str>::None])?;
                }
                // Inhibitor substances
                for subst in &inhib_subst {
                    stmt.execute(params![enzyme, pattern, "inhibitor", Option::<&str>::None, subst])?;
                }
                // Inducer ATC prefixes
                for atc in &induc_atc {
                    stmt.execute(params![enzyme, pattern, "inducer", atc, Option::<&str>::None])?;
                }
                // Inducer substances
                for subst in &induc_subst {
                    stmt.execute(params![enzyme, pattern, "inducer", Option::<&str>::None, subst])?;
                }
            }
        }
    }

    conn.execute_batch("COMMIT")?;
    Ok(())
}

fn search_interactions(db_path: &str, term: &str, limit: Option<usize>) -> Result<()> {
    let conn = Connection::open(db_path)?;
    let pattern = format!("%{}%", term);

    let query = if limit.is_some() {
        "SELECT drug_brand, drug_substance, interacting_substance, interacting_brands, \
         description, severity_score, severity_label \
         FROM interactions WHERE description LIKE ?1 \
         ORDER BY severity_score DESC LIMIT ?2"
    } else {
        "SELECT drug_brand, drug_substance, interacting_substance, interacting_brands, \
         description, severity_score, severity_label \
         FROM interactions WHERE description LIKE ?1 \
         ORDER BY severity_score DESC"
    };
    let mut stmt = conn.prepare(query)?;

    let rows: Vec<(String, String, String, String, String, u8, String)> = if let Some(lim) = limit {
        stmt.query_map(params![pattern, lim], |row| {
            Ok((
                row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                row.get(4)?, row.get(5)?, row.get(6)?,
            ))
        })?.filter_map(|r| r.ok()).collect()
    } else {
        stmt.query_map(params![pattern], |row| {
            Ok((
                row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?,
                row.get(4)?, row.get(5)?, row.get(6)?,
            ))
        })?.filter_map(|r| r.ok()).collect()
    };

    if rows.is_empty() {
        println!("No interactions found matching \"{}\".", term);
        return Ok(());
    }

    if limit.is_some() {
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM interactions WHERE description LIKE ?1",
            params![pattern],
            |row| row.get(0),
        )?;
        println!(
            "Found {} interactions matching \"{}\" (showing top {}):\n",
            total, term, rows.len()
        );
    } else {
        println!(
            "Found {} interactions matching \"{}\":\n",
            rows.len(), term
        );
    }

    for (drug_brand, _drug_substance, interacting_substance, interacting_brands, desc, sev_score, sev_label) in &rows {
        let other_brands = if interacting_brands.is_empty() {
            String::new()
        } else {
            // Show first brand only to keep output concise
            let first = interacting_brands.split(", ").next().unwrap_or("");
            format!(" ({})", first)
        };
        println!(
            "[Swissmedic FI] {} <-> {}{} | Severity: {} ({})",
            drug_brand,
            interacting_substance,
            other_brands,
            severity_indicator(*sev_score),
            sev_label
        );
        println!("  {}\n", desc);
    }

    // Also search EPha interactions
    let epha_query = if limit.is_some() {
        "SELECT title, effect, mechanism, measures, risk_class, risk_label, severity_score \
         FROM epha_interactions WHERE effect LIKE ?1 OR mechanism LIKE ?1 OR measures LIKE ?1 \
         ORDER BY severity_score DESC LIMIT ?2"
    } else {
        "SELECT title, effect, mechanism, measures, risk_class, risk_label, severity_score \
         FROM epha_interactions WHERE effect LIKE ?1 OR mechanism LIKE ?1 OR measures LIKE ?1 \
         ORDER BY severity_score DESC"
    };
    let mut epha_stmt = conn.prepare(epha_query)?;
    let epha_rows: Vec<(String, String, String, String, String, String, u8)> = if let Some(lim) = limit {
        epha_stmt.query_map(params![pattern, lim], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))
        })?.filter_map(|r| r.ok()).collect()
    } else {
        epha_stmt.query_map(params![pattern], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))
        })?.filter_map(|r| r.ok()).collect()
    };

    if !epha_rows.is_empty() {
        println!("--- EPha results ({}) ---\n", epha_rows.len());
        for (title, effect, mechanism, measures, risk_class, _risk_label, _sev) in &epha_rows {
            println!("[EPha] {} | Risikoklasse: {}", title, risk_class);
            println!("  Effekt: {}", effect);
            println!("  Mechanismus: {}", mechanism);
            println!("  Massnahmen: {}\n", measures);
        }
    }

    Ok(())
}

fn list_class_interactions(db_path: &str) -> Result<()> {
    let conn = Connection::open(db_path)?;

    // Load all drugs with ATC codes and interaction texts
    let mut stmt = conn.prepare(
        "SELECT brand_name, atc_code, active_substances, interactions_text FROM drugs WHERE length(interactions_text) > 0 AND atc_code IS NOT NULL AND atc_code != ''"
    )?;
    struct DrugRow { _brand: String, atc: String, substances: String, text: String }
    let drugs: Vec<DrugRow> = stmt
        .query_map([], |row| {
            Ok(DrugRow {
                _brand: row.get::<_, String>(0)?,
                atc: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                substances: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                text: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
            })
        })?
        .filter_map(|r| r.ok())
        .filter(|d| !d.atc.is_empty() && !d.text.is_empty())
        .collect();

    let total = drugs.len();
    eprintln!("Scanning {} drugs for class-level interactions...", total);

    let class_keywords = load_class_keywords(&conn);

    // For each ATC class, count: how many drugs belong to it, how many OTHER drugs mention its keywords
    // Then show unique substance-pair level class interactions

    // Group drugs by ATC prefix
    let mut drugs_in_class: HashMap<String, usize> = HashMap::new();
    for (prefix, _) in &class_keywords {
        let count = drugs.iter().filter(|d| d.atc.starts_with(prefix.as_str())).count();
        drugs_in_class.insert(prefix.to_string(), count);
    }

    // For each class, find drugs that mention its keywords (but don't belong to the class themselves)
    let mut total_interactions = 0u64;
    let mut class_results: Vec<(String, usize, usize, u64, String)> = Vec::new(); // (prefix, drugs_in_class, drugs_mentioning, pair_count, best_keyword)

    for (prefix, keywords) in &class_keywords {
        let n_in_class = *drugs_in_class.get(prefix.as_str()).unwrap_or(&0);
        if n_in_class == 0 {
            continue;
        }

        // Unique substances mentioning keywords (that are NOT in this class)
        let mut mentioning_substances: HashSet<String> = HashSet::new();
        let mut best_keyword = String::new();
        let mut best_count = 0usize;

        for kw in keywords {
            let mut count = 0usize;
            for drug in &drugs {
                if drug.atc.starts_with(prefix.as_str()) {
                    continue; // Skip drugs in the same class
                }
                let text_lower = drug.text.to_lowercase();
                if text_lower.contains(kw.as_str()) {
                    mentioning_substances.insert(drug.substances.clone());
                    count += 1;
                }
            }
            if count > best_count {
                best_count = count;
                best_keyword = kw.to_string();
            }
        }

        let n_mentioning = mentioning_substances.len();
        // Each mentioning substance group × each drug in class = potential class-level interaction pairs
        let pair_count = (n_mentioning as u64) * (n_in_class as u64);
        total_interactions += pair_count;
        class_results.push((prefix.to_string(), n_in_class, n_mentioning, pair_count, best_keyword));
    }

    // Sort by pair count descending
    class_results.sort_by(|a, b| b.3.cmp(&a.3));

    println!("{:<10} {:>12} {:>18} {:>18}   {}", "ATC Class", "Drugs in Cl.", "Drugs Mentioning", "Potential Pairs", "Top Keyword");
    println!("{}", "-".repeat(90));
    for (prefix, n_in, n_mention, pairs, keyword) in &class_results {
        println!("{:<10} {:>12} {:>18} {:>18}   {}", prefix, n_in, n_mention, pairs, keyword);
    }
    println!("{}", "-".repeat(90));
    println!("Total potential class-level interaction pairs: {}", total_interactions);

    Ok(())
}

fn basket_check(db_path: &str, basket: &[&str]) -> Result<()> {
    let conn = Connection::open(db_path)?;

    let mut basket_drugs: Vec<BasketDrug> = Vec::new();
    for input in basket {
        // Try brand name first, then fall back to substance name
        let mut stmt = conn.prepare(
            "SELECT brand_name, active_substances, atc_code, atc_class, interactions_text FROM drugs WHERE brand_name LIKE ?1 ORDER BY length(interactions_text) DESC",
        )?;
        let pattern = format!("%{}%", input);
        let mut rows = stmt.query(params![pattern])?;
        let found = if let Some(row) = rows.next()? {
            Some((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            ))
        } else {
            drop(rows);
            drop(stmt);
            // Search by substance name via substance_brand_map
            let mut stmt2 = conn.prepare(
                "SELECT DISTINCT d.brand_name, d.active_substances, d.atc_code, d.atc_class, d.interactions_text \
                 FROM substance_brand_map s JOIN drugs d ON d.brand_name = s.brand_name \
                 WHERE s.substance LIKE ?1 ORDER BY length(d.interactions_text) DESC LIMIT 1",
            )?;
            let pattern_lower = format!("%{}%", input.to_lowercase());
            let mut rows2 = stmt2.query(params![pattern_lower])?;
            if let Some(row) = rows2.next()? {
                Some((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                ))
            } else {
                None
            }
        };

        if let Some((brand_name, substances_str, atc_code, atc_class, interactions_text)) = found {
            let substances: Vec<String> = substances_str
                .split(", ")
                .map(|s| s.to_lowercase())
                .collect();

            basket_drugs.push(BasketDrug {
                brand: brand_name,
                substances,
                atc_code,
                atc_class,
                interactions_text,
            });
        } else {
            println!("  Not found: {}", input);
        }
    }

    println!("Basket contents:");
    for drug in &basket_drugs {
        println!(
            "  {} [{}] -> {}",
            drug.brand,
            drug.atc_code,
            drug.substances.join(", ")
        );
    }

    // Load detection rules once from DB
    let class_keywords = load_class_keywords(&conn);
    let cyp_rules = load_cyp_rules(&conn);

    // Check all pairs
    let mut found_any = false;
    for i in 0..basket_drugs.len() {
        for j in (i + 1)..basket_drugs.len() {
            let a = &basket_drugs[i];
            let b = &basket_drugs[j];

            // Strategy 1: DB substance match (A's text mentions B's substance)
            for subst in &b.substances {
                let mut stmt = conn.prepare(
                    "SELECT description, severity_score, severity_label FROM interactions WHERE drug_brand = ?1 AND interacting_substance = ?2",
                )?;
                let rows: Vec<(String, u8, String)> = stmt
                    .query_map(params![a.brand, subst], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                for (desc, sev_score, sev_label) in &rows {
                    println!(
                        "\nINTERACTION [substance match]: {} <-> {} | Severity: {} ({})",
                        a.brand, b.brand, severity_indicator(*sev_score), sev_label
                    );
                    println!("  Via substance: {}", subst);
                    println!("  {}", desc);
                    found_any = true;
                }
            }

            // Reverse: B's text mentions A's substance
            for subst in &a.substances {
                let mut stmt = conn.prepare(
                    "SELECT description, severity_score, severity_label FROM interactions WHERE drug_brand = ?1 AND interacting_substance = ?2",
                )?;
                let rows: Vec<(String, u8, String)> = stmt
                    .query_map(params![b.brand, subst], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                for (desc, sev_score, sev_label) in &rows {
                    println!(
                        "\nINTERACTION [substance match]: {} <-> {} | Severity: {} ({})",
                        b.brand, a.brand, severity_indicator(*sev_score), sev_label
                    );
                    println!("  Via substance: {}", subst);
                    println!("  {}", desc);
                    found_any = true;
                }
            }

            // Strategy 2: Full-text search for class-level interactions
            // Search A's interaction text for keywords related to B's drug class
            let class_hits_ab = find_class_interactions(&a.interactions_text, &b.atc_code, &class_keywords);
            for hit in &class_hits_ab {
                let (sev_score, sev_label) = score_severity(&hit.context);
                println!(
                    "\nINTERACTION [class-level]: {} <-> {} ({}) | Severity: {} ({})",
                    a.brand, b.brand, hit.class_keyword, severity_indicator(sev_score), sev_label
                );
                println!("  {}", hit.context);
                found_any = true;
            }

            let class_hits_ba = find_class_interactions(&b.interactions_text, &a.atc_code, &class_keywords);
            for hit in &class_hits_ba {
                let (sev_score, sev_label) = score_severity(&hit.context);
                println!(
                    "\nINTERACTION [class-level]: {} <-> {} ({}) | Severity: {} ({})",
                    b.brand, a.brand, hit.class_keyword, severity_indicator(sev_score), sev_label
                );
                println!("  {}", hit.context);
                found_any = true;
            }

            // Strategy 3: CYP enzyme interactions
            // Check if A's text mentions a CYP enzyme and B is a known inhibitor/inducer
            let cyp_hits_ab = find_cyp_interactions(&a.interactions_text, &b.atc_code, &b.substances, &cyp_rules);
            for hit in &cyp_hits_ab {
                let (sev_score, sev_label) = score_severity(&hit.context);
                println!(
                    "\nINTERACTION [CYP]: {} <-> {} ({}) | Severity: {} ({})",
                    a.brand, b.brand, hit.class_keyword, severity_indicator(sev_score), sev_label
                );
                println!("  {}", hit.context);
                found_any = true;
            }

            let cyp_hits_ba = find_cyp_interactions(&b.interactions_text, &a.atc_code, &a.substances, &cyp_rules);
            for hit in &cyp_hits_ba {
                let (sev_score, sev_label) = score_severity(&hit.context);
                println!(
                    "\nINTERACTION [CYP]: {} <-> {} ({}) | Severity: {} ({})",
                    b.brand, a.brand, hit.class_keyword, severity_indicator(sev_score), sev_label
                );
                println!("  {}", hit.context);
                found_any = true;
            }

            // Strategy 4: EPha curated interactions by ATC pair
            let mut epha_stmt = conn.prepare(
                "SELECT risk_class, risk_label, effect, mechanism, measures, severity_score, title \
                 FROM epha_interactions WHERE (atc1 = ?1 AND atc2 = ?2) OR (atc1 = ?2 AND atc2 = ?1) LIMIT 1",
            )?;
            let epha_rows: Vec<(String, String, String, String, String, u8, String)> = epha_stmt
                .query_map(params![a.atc_code, b.atc_code], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })?
                .filter_map(|r| r.ok())
                .collect();
            for (risk_class, risk_label, effect, mechanism, measures, sev_score, _title) in &epha_rows {
                println!(
                    "\nINTERACTION [EPha]: {} <-> {} | Risikoklasse: {} ({})",
                    a.brand, b.brand, risk_class, risk_label
                );
                println!("  Effekt: {}", effect);
                println!("  Mechanismus: {}", mechanism);
                println!("  Massnahmen: {}", measures);
                let _ = sev_score; // used for display ordering
                found_any = true;
            }
        }
    }

    if !found_any {
        println!("\nNo interactions found between basket drugs.");
    }

    Ok(())
}

pub struct ClassHit {
    pub class_keyword: String,
    pub context: String,
}

/// Parse ATC class keywords from txt/keywords.txt (embedded at compile time).
/// Used during build to populate the class_keywords table.
fn parse_class_keywords() -> Vec<(String, Vec<String>)> {
    let raw = include_str!("../txt/keywords.txt");
    let mut result = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((prefix, rest)) = line.split_once('\t') {
            let keywords: Vec<String> = rest.split(',').map(|s| s.trim().to_string()).collect();
            result.push((prefix.trim().to_string(), keywords));
        }
    }
    result
}

/// CYP rule data used during build to populate the cyp_rules table.
/// Returns (enzyme, text_patterns, inhibitor_atc, inhibitor_substances, inducer_atc, inducer_substances).
fn cyp_rule_data() -> Vec<(&'static str, Vec<&'static str>, Vec<&'static str>, Vec<&'static str>, Vec<&'static str>, Vec<&'static str>)> {
    vec![
        ("CYP3A4",
         vec!["cyp3a4", "cyp3a"],
         vec!["J05AE", "J02A", "J01FA"],
         vec!["ritonavir", "cobicistat", "itraconazol", "ketoconazol", "voriconazol",
              "posaconazol", "fluconazol", "clarithromycin", "erythromycin",
              "diltiazem", "verapamil", "grapefruit"],
         vec!["J04AB", "N03AF", "N03AB"],
         vec!["rifampicin", "rifabutin", "carbamazepin", "phenytoin", "phenobarbital",
              "johanniskraut", "efavirenz", "nevirapin"]),
        ("CYP2D6",
         vec!["cyp2d6"],
         vec![],
         vec!["fluoxetin", "paroxetin", "bupropion", "chinidin", "terbinafin",
              "duloxetin", "ritonavir", "cobicistat"],
         vec![],
         vec!["rifampicin"]),
        ("CYP2C9",
         vec!["cyp2c9"],
         vec![],
         vec!["fluconazol", "amiodaron", "miconazol", "voriconazol", "fluvoxamin"],
         vec![],
         vec!["rifampicin", "carbamazepin", "phenytoin"]),
        ("CYP2C19",
         vec!["cyp2c19"],
         vec![],
         vec!["omeprazol", "esomeprazol", "fluvoxamin", "fluconazol", "voriconazol",
              "ticlopidin"],
         vec![],
         vec!["rifampicin", "carbamazepin", "phenytoin", "johanniskraut"]),
        ("CYP1A2",
         vec!["cyp1a2"],
         vec!["J01MA"],
         vec!["ciprofloxacin", "fluvoxamin", "enoxacin"],
         vec![],
         vec!["rifampicin", "carbamazepin", "phenytoin", "johanniskraut"]),
        ("CYP2C8",
         vec!["cyp2c8"],
         vec![],
         vec!["gemfibrozil", "clopidogrel", "trimethoprim"],
         vec![],
         vec!["rifampicin"]),
        ("CYP2B6",
         vec!["cyp2b6"],
         vec![],
         vec!["ticlopidin", "clopidogrel"],
         vec![],
         vec!["rifampicin", "efavirenz"]),
    ]
}

/// Load class keywords from the interactions DB.
pub fn load_class_keywords(conn: &Connection) -> Vec<(String, Vec<String>)> {
    let mut stmt = conn
        .prepare("SELECT atc_prefix, keyword FROM class_keywords ORDER BY atc_prefix")
        .expect("class_keywords table missing");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("failed to query class_keywords")
        .filter_map(|r| r.ok())
        .collect();

    let mut map: Vec<(String, Vec<String>)> = Vec::new();
    for (prefix, keyword) in rows {
        if let Some(last) = map.last_mut() {
            if last.0 == prefix {
                last.1.push(keyword);
                continue;
            }
        }
        map.push((prefix, vec![keyword]));
    }
    map
}

/// CYP rule loaded from the DB, grouped by enzyme.
pub struct CypRule {
    pub enzyme: String,
    pub text_patterns: Vec<String>,
    pub inhibitor_atc: Vec<String>,
    pub inhibitor_substances: Vec<String>,
    pub inducer_atc: Vec<String>,
    pub inducer_substances: Vec<String>,
}

/// Load CYP rules from the interactions DB.
pub fn load_cyp_rules(conn: &Connection) -> Vec<CypRule> {
    let mut stmt = conn
        .prepare("SELECT enzyme, text_pattern, role, atc_prefix, substance FROM cyp_rules ORDER BY enzyme")
        .expect("cyp_rules table missing");
    let rows: Vec<(String, String, String, Option<String>, Option<String>)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))
        .expect("failed to query cyp_rules")
        .filter_map(|r| r.ok())
        .collect();

    let mut map: HashMap<String, CypRule> = HashMap::new();
    for (enzyme, text_pattern, role, atc_prefix, substance) in rows {
        let rule = map.entry(enzyme.clone()).or_insert_with(|| CypRule {
            enzyme,
            text_patterns: Vec::new(),
            inhibitor_atc: Vec::new(),
            inhibitor_substances: Vec::new(),
            inducer_atc: Vec::new(),
            inducer_substances: Vec::new(),
        });
        if !rule.text_patterns.contains(&text_pattern) {
            rule.text_patterns.push(text_pattern);
        }
        match role.as_str() {
            "inhibitor" => {
                if let Some(atc) = atc_prefix {
                    if !rule.inhibitor_atc.contains(&atc) {
                        rule.inhibitor_atc.push(atc);
                    }
                }
                if let Some(subst) = substance {
                    if !rule.inhibitor_substances.contains(&subst) {
                        rule.inhibitor_substances.push(subst);
                    }
                }
            }
            "inducer" => {
                if let Some(atc) = atc_prefix {
                    if !rule.inducer_atc.contains(&atc) {
                        rule.inducer_atc.push(atc);
                    }
                }
                if let Some(subst) = substance {
                    if !rule.inducer_substances.contains(&subst) {
                        rule.inducer_substances.push(subst);
                    }
                }
            }
            _ => {}
        }
    }
    map.into_values().collect()
}

/// Search a drug's interaction text for class-level keywords that match the other drug.
pub fn find_class_interactions(interaction_text: &str, other_atc: &str, class_keywords: &[(String, Vec<String>)]) -> Vec<ClassHit> {
    let text_lower = interaction_text.to_lowercase();
    let mut hits = Vec::new();

    for (atc_prefix, keywords) in class_keywords {
        if !other_atc.starts_with(atc_prefix.as_str()) {
            continue;
        }

        for keyword in keywords {
            if text_lower.contains(keyword.as_str()) {
                let context = extract_context(interaction_text, keyword);
                if !context.is_empty() {
                    hits.push(ClassHit {
                        class_keyword: keyword.to_string(),
                        context,
                    });
                    break; // One hit per ATC prefix is enough
                }
            }
        }
    }

    hits
}

/// CYP enzyme interaction detection using pre-loaded rules from the DB.
pub fn find_cyp_interactions(interaction_text: &str, other_atc: &str, other_substances: &[String], cyp_rules: &[CypRule]) -> Vec<ClassHit> {
    let text_lower = interaction_text.to_lowercase();
    let mut hits = Vec::new();

    let other_subst_lower: Vec<String> = other_substances.iter().map(|s| s.to_lowercase()).collect();

    for rule in cyp_rules {
        // Check if interaction text mentions this CYP enzyme
        let mentioned = rule.text_patterns.iter().any(|p| text_lower.contains(p.as_str()));
        if !mentioned {
            continue;
        }

        // Check if the other drug is a known inhibitor
        let is_inhibitor = rule.inhibitor_atc.iter().any(|prefix| other_atc.starts_with(prefix.as_str()))
            || rule.inhibitor_substances.iter().any(|s| other_subst_lower.iter().any(|os| os == s));

        // Check if the other drug is a known inducer
        let is_inducer = rule.inducer_atc.iter().any(|prefix| other_atc.starts_with(prefix.as_str()))
            || rule.inducer_substances.iter().any(|s| other_subst_lower.iter().any(|os| os == s));

        if is_inhibitor || is_inducer {
            let role = if is_inhibitor { "Hemmer" } else { "Induktor" };
            let pattern = &rule.text_patterns[0];
            let context = extract_context(interaction_text, pattern);
            if !context.is_empty() {
                hits.push(ClassHit {
                    class_keyword: format!("{}-{}", rule.enzyme, role),
                    context,
                });
            }
        }
    }

    hits
}

struct BasketDrug {
    brand: String,
    substances: Vec<String>,
    atc_code: String,
    #[allow(dead_code)]
    atc_class: String,
    interactions_text: String,
}
