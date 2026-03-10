use aho_corasick::AhoCorasick;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use regex::Regex;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

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
    },
    /// Check interactions between drugs in a basket
    Check {
        /// Brand names or substance names of drugs to check (e.g. Ponstan Marcoumar Aspirin)
        #[arg(required = true)]
        drugs: Vec<String>,
    },
    /// Search interactions by clinical term (e.g. Prothrombinzeit, QT-Verlängerung, Blutungsrisiko)
    Search {
        /// Search term to find in interaction descriptions
        #[arg(required = true)]
        term: String,
        /// Maximum number of results to show
        #[arg(short, long, default_value = "20")]
        limit: usize,
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    let db_path = "db/amiko_db_full_idx_de.db";
    let output_path = "db/interactions.db";

    match cli.command {
        Some(Commands::Check { drugs }) => {
            let drug_refs: Vec<&str> = drugs.iter().map(|s| s.as_str()).collect();
            basket_check(output_path, &drug_refs)?;
        }
        Some(Commands::Search { term, limit }) => {
            search_interactions(output_path, &term, limit)?;
        }
        Some(Commands::Build { download }) => {
            if download {
                download_source_db(db_path)?;
                download_atc_csv()?;
            }
            run_build(db_path, output_path)?;
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

    write_interactions_db(output_path, &drugs, &interactions, &substance_to_brands)?;
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
fn score_severity(text: &str) -> (u8, &'static str) {
    let lower = text.to_lowercase();

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
        if lower.contains(kw) {
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
        "erhöht",
        "erniedrigt",
        "beeinflusst",
        "wechselwirkung",
        "plasmaspiegel",
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

fn severity_indicator(score: u8) -> &'static str {
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

        if sev > best_severity || best_snippet.is_empty() {
            best_severity = sev;
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
            interactions_text TEXT
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
            brand_name TEXT NOT NULL
        );
        CREATE INDEX idx_sbm_substance ON substance_brand_map(substance);
        CREATE INDEX idx_sbm_brand ON substance_brand_map(brand_name);
        ",
    )?;

    conn.execute_batch("BEGIN TRANSACTION")?;

    {
        let mut stmt = conn.prepare(
            "INSERT INTO drugs (id, brand_name, atc_code, atc_class, active_substances, interactions_text) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        for drug in drugs {
            stmt.execute(params![
                drug.id,
                drug.title,
                drug.atc_code,
                drug.atc_class,
                drug.active_substances.join(", "),
                drug.interactions_text,
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
            .prepare("INSERT INTO substance_brand_map (substance, brand_name) VALUES (?1, ?2)")?;
        for (substance, brands) in substance_to_brands {
            for brand in brands {
                stmt.execute(params![substance, brand])?;
            }
        }
    }

    conn.execute_batch("COMMIT")?;
    Ok(())
}

fn search_interactions(db_path: &str, term: &str, limit: usize) -> Result<()> {
    let conn = Connection::open(db_path)?;
    let pattern = format!("%{}%", term);

    let mut stmt = conn.prepare(
        "SELECT drug_brand, drug_substance, interacting_substance, interacting_brands, \
         description, severity_score, severity_label \
         FROM interactions WHERE description LIKE ?1 \
         ORDER BY severity_score DESC LIMIT ?2",
    )?;

    let rows: Vec<(String, String, String, String, String, u8, String)> = stmt
        .query_map(params![pattern, limit], |row| {
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

    if rows.is_empty() {
        println!("No interactions found matching \"{}\".", term);
        return Ok(());
    }

    // Also count total matches
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM interactions WHERE description LIKE ?1",
        params![pattern],
        |row| row.get(0),
    )?;

    println!(
        "Found {} interactions matching \"{}\" (showing top {}):\n",
        total,
        term,
        rows.len()
    );

    for (drug_brand, _drug_substance, interacting_substance, interacting_brands, desc, sev_score, sev_label) in &rows {
        let other_brands = if interacting_brands.is_empty() {
            String::new()
        } else {
            // Show first brand only to keep output concise
            let first = interacting_brands.split(", ").next().unwrap_or("");
            format!(" ({})", first)
        };
        println!(
            "{} <-> {}{} | Severity: {} ({})",
            drug_brand,
            interacting_substance,
            other_brands,
            severity_indicator(*sev_score),
            sev_label
        );
        println!("  {}\n", desc);
    }

    Ok(())
}

fn basket_check(db_path: &str, basket: &[&str]) -> Result<()> {
    let conn = Connection::open(db_path)?;

    let mut basket_drugs: Vec<BasketDrug> = Vec::new();
    for input in basket {
        // Try brand name first, then fall back to substance name
        let mut stmt = conn.prepare(
            "SELECT brand_name, active_substances, atc_code, atc_class, interactions_text FROM drugs WHERE brand_name LIKE ?1",
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
                 WHERE s.substance LIKE ?1 LIMIT 1",
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
            let class_hits_ab = find_class_interactions(&a.interactions_text, &b.atc_code);
            for hit in &class_hits_ab {
                let (sev_score, sev_label) = score_severity(&hit.context);
                println!(
                    "\nINTERACTION [class-level]: {} <-> {} ({}) | Severity: {} ({})",
                    a.brand, b.brand, hit.class_keyword, severity_indicator(sev_score), sev_label
                );
                println!("  {}", hit.context);
                found_any = true;
            }

            let class_hits_ba = find_class_interactions(&b.interactions_text, &a.atc_code);
            for hit in &class_hits_ba {
                let (sev_score, sev_label) = score_severity(&hit.context);
                println!(
                    "\nINTERACTION [class-level]: {} <-> {} ({}) | Severity: {} ({})",
                    b.brand, a.brand, hit.class_keyword, severity_indicator(sev_score), sev_label
                );
                println!("  {}", hit.context);
                found_any = true;
            }
        }
    }

    if !found_any {
        println!("\nNo interactions found between basket drugs.");
    }

    Ok(())
}

struct ClassHit {
    class_keyword: String,
    context: String,
}

/// Search a drug's interaction text for class-level keywords that match the other drug.
/// Maps ATC classes to keywords that appear in interaction texts.
fn find_class_interactions(interaction_text: &str, other_atc: &str) -> Vec<ClassHit> {
    let text_lower = interaction_text.to_lowercase();
    let mut hits = Vec::new();

    let class_keywords: &[(&str, &[&str])] = &[
        // B01A = Antithrombotische Mittel / Antikoagulantien
        ("B01A", &["antikoagul", "warfarin", "cumarin", "coumarin", "vitamin-k-antagonist",
                    "vitamin k antagonist", "blutgerinnungshemm", "thrombozytenaggregationshemm",
                    "plättchenhemm", "antithrombotisch", "heparin", "thrombin-hemm",
                    "faktor-xa", "direktes orales antikoagulans", "doak"]),
        // B01AC = Thrombozytenaggregationshemmer (ASS, Clopidogrel)
        ("B01AC", &["thrombozytenaggregationshemm", "plättchenhemm", "thrombocytenaggregation"]),
        // M01A = Nichtsteroidale Antiphlogistika (NSAIDs/NSAR)
        ("M01A", &["nsar", "nsaid", "nichtsteroidale antiphlogistika", "antiphlogistika",
                    "nichtsteroidale antirheumatika", "cox-2", "cox-hemmer", "cyclooxygenase",
                    "prostaglandinsynthesehemm", "entzündungshemm"]),
        // N02B = Andere Analgetika und Antipyretika (incl. ASS)
        ("N02B", &["analgetik", "antipyretik", "acetylsalicylsäure", "paracetamol"]),
        // N02A = Opioide
        ("N02A", &["opioid", "opiat", "morphin", "atemdepression", "zns-depression"]),
        // C09A/C09B = ACE-Hemmer
        ("C09A", &["ace-hemmer", "ace-inhibitor", "ace inhibitor", "angiotensin-converting"]),
        ("C09B", &["ace-hemmer", "ace-inhibitor", "angiotensin-converting"]),
        // C09C/C09D = Angiotensin-II-Antagonisten (Sartane)
        ("C09C", &["angiotensin", "sartan", "at1-rezeptor", "at1-antagonist", "at1-blocker"]),
        ("C09D", &["angiotensin", "sartan", "at1-rezeptor", "at1-antagonist"]),
        // C07 = Beta-Blocker
        ("C07", &["beta-blocker", "betablocker", "β-blocker", "betarezeptorenblocker",
                   "beta-adrenozeptor"]),
        // C08 = Calciumkanalblocker
        ("C08", &["calciumantagonist", "calciumkanalblocker", "kalziumantagonist",
                   "kalziumkanalblocker", "calcium-antagonist"]),
        // C03 = Diuretika
        ("C03", &["diuretik", "thiazid", "schleifendiuretik", "kaliumsparend"]),
        // C03C = Schleifendiuretika
        ("C03C", &["schleifendiuretik", "furosemid", "torasemid"]),
        // C03A = Thiazide
        ("C03A", &["thiazid", "hydrochlorothiazid"]),
        // C01A = Herzglykoside (Digoxin)
        ("C01A", &["herzglykosid", "digoxin", "digitalis", "digitoxin"]),
        // C01B = Antiarrhythmika
        ("C01B", &["antiarrhythmi", "amiodaron"]),
        // C10A = Statine (Lipidsenker)
        ("C10A", &["statin", "hmg-coa", "lipidsenk", "cholesterinsenk"]),
        // N06AB = SSRIs
        ("N06AB", &["ssri", "serotonin-wiederaufnahme", "serotonin reuptake",
                     "selektive serotonin", "serotonerg"]),
        // N06A = Antidepressiva allgemein
        ("N06A", &["antidepressiv", "trizyklisch", "serotonin", "snri", "maoh",
                    "mao-hemmer", "monoaminoxidase"]),
        // A10 = Antidiabetika
        ("A10", &["antidiabetik", "insulin", "blutzucker", "hypoglykämie", "orale antidiabetika",
                   "sulfonylharnstoff", "metformin"]),
        // H02 = Corticosteroide
        ("H02", &["corticosteroid", "kortikosteroid", "glucocorticoid", "glukokortikoid",
                   "kortison", "steroid"]),
        // L04 = Immunsuppressiva
        ("L04", &["immunsuppress", "ciclosporin", "tacrolimus", "mycophenolat", "azathioprin",
                   "sirolimus"]),
        // L01 = Antineoplastische Mittel
        ("L01", &["antineoplast", "zytostatik", "methotrexat", "chemotherap"]),
        // N03 = Antiepileptika
        ("N03", &["antiepileptik", "antikonvulsiv", "krampflösend", "carbamazepin",
                   "valproinsäure", "phenytoin", "enzymindukt"]),
        // N05A = Antipsychotika
        ("N05A", &["antipsychoti", "neuroleptik", "qt-verlänger", "qt-zeit"]),
        // N05B/N05C = Anxiolytika / Sedativa
        ("N05B", &["anxiolytik", "benzodiazepin"]),
        ("N05C", &["sedativ", "hypnotik", "schlafmittel", "zns-dämfpend", "zns-depression"]),
        // J01 = Antibiotika
        ("J01", &["antibiotik", "antibakteriell"]),
        // J01FA = Makrolide
        ("J01FA", &["makrolid", "erythromycin", "clarithromycin", "azithromycin"]),
        // J01MA = Fluorchinolone
        ("J01MA", &["fluorchinolon", "chinolon", "gyrasehemm"]),
        // J02A = Antimykotika systemisch
        ("J02A", &["antimykotik", "azol-antimykotik", "triazol", "itraconazol",
                    "fluconazol", "voriconazol", "cyp3a4-hemm"]),
        // J05A = Antivirale
        ("J05A", &["antiviral", "proteasehemm", "protease-inhibitor", "hiv"]),
        // A02BC = Protonenpumpeninhibitoren (PPI)
        ("A02BC", &["protonenpumpeninhibitor", "protonenpumpenhemm", "ppi", "säureblocker"]),
        // A02B = Ulkusmittel
        ("A02B", &["antazid", "h2-blocker", "h2-antagonist", "säurehemm"]),
        // G03A = Hormonale Kontrazeptiva
        ("G03A", &["kontrazeptiv", "östrogen", "orale kontrazeptiva", "hormonelle verhütung"]),
        // N07 = Andere Mittel für das Nervensystem
        ("N07", &["dopaminerg", "cholinerg", "anticholinerg"]),
        // R03 = Mittel bei obstruktiven Atemwegserkrankungen
        ("R03", &["bronchodilatat", "theophyllin", "sympathomimetik", "beta-2"]),
        // M04 = Gichtmittel
        ("M04", &["urikosurik", "gichtmittel", "harnsäure", "allopurinol"]),
        // B03 = Antianämika
        ("B03", &["eisen", "eisenpräparat", "eisensupplementation"]),
        // L02BA = Antiöstrogene / SERMs (Tamoxifen, Toremifen)
        ("L02BA", &["toremifen", "tamoxifen", "antiöstrogen", "östrogen-rezeptor",
                     "serm", "selektive östrogenrezeptor"]),
        // L02B = Hormonantagonisten
        ("L02B", &["hormonantagonist", "antihormon", "antiandrogen", "antiöstrogen"]),
        // V03AB = Antidota (Sugammadex etc.)
        ("V03AB", &["sugammadex", "antidot", "antagonisierung", "neuromuskuläre blockade",
                     "verdrängung"]),
        // M03A = Muskelrelaxantien, peripher wirkend
        ("M03A", &["muskelrelax", "neuromuskulär", "rocuronium", "vecuronium",
                    "succinylcholin", "curare"]),
    ];

    for &(atc_prefix, keywords) in class_keywords {
        if !other_atc.starts_with(atc_prefix) {
            continue;
        }

        for &keyword in keywords {
            if text_lower.contains(keyword) {
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

struct BasketDrug {
    brand: String,
    substances: Vec<String>,
    atc_code: String,
    #[allow(dead_code)]
    atc_class: String,
    interactions_text: String,
}
