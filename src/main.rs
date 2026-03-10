use aho_corasick::AhoCorasick;
use anyhow::{Context, Result};
use regex::Regex;
use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};

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
    let db_path = "db/amiko_db_full_idx_de.db";
    let output_path = "db/interactions.db";

    println!("=== Swiss Drug Interaction Finder (SDIF) ===");
    println!("Reading source database: {}", db_path);

    let source = Connection::open(db_path)
        .with_context(|| format!("Failed to open source DB: {}", db_path))?;

    let drugs = parse_all_drugs(&source)?;
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

    println!("\n--- Demo: Ponstan + Marcoumar ---");
    demo_basket_check(output_path, &["Ponstan", "Marcoumar"])?;

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

    for (i, &(_, start)) in positions.iter().enumerate() {
        let end = if i + 1 < positions.len() {
            let next = positions[i + 1].1;
            content[..next].rfind("<div").unwrap_or(next)
        } else {
            content.len()
        };
        let text = strip_html(&content[start..end]);
        if text.starts_with("Interaktionen") {
            return text;
        }
    }
    String::new()
}

fn parse_all_drugs(conn: &Connection) -> Result<Vec<Drug>> {
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

fn is_common_word(s: &str) -> bool {
    matches!(
        s,
        "oder" | "anti" | "wasser" | "wirkstoffe" | "nicht" | "aber" | "auch"
            | "wird" | "kann" | "sind" | "eine" | "dies" | "nach" | "über"
            | "mehr" | "alle" | "dazu" | "etwa" | "noch" | "hier" | "sehr"
            | "gabe" | "glas" | "darm" | "laut" | "teil" | "fall" | "form"
    )
}

fn extract_context(text: &str, substance: &str) -> String {
    let lower = text.to_lowercase();
    if let Some(pos) = lower.find(substance) {
        let start = lower[..pos]
            .rfind(|c: char| c == '.' || c == ':')
            .map(|p| p + 1)
            .unwrap_or(0);
        let end = lower[pos..]
            .find('.')
            .map(|p| pos + p + 1)
            .unwrap_or(text.len());

        let snippet = text[start..end.min(text.len())].trim();
        if snippet.len() > 500 {
            let mut trunc = 497;
            while !snippet.is_char_boundary(trunc) && trunc > 0 {
                trunc -= 1;
            }
            format!("{}...", &snippet[..trunc])
        } else {
            snippet.to_string()
        }
    } else {
        String::new()
    }
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
            description TEXT NOT NULL
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
            "INSERT INTO interactions (drug_brand, drug_substance, interacting_substance, interacting_brands, description) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for interaction in interactions {
            let interacting_brands = substance_to_brands
                .get(&interaction.interacting_substance)
                .map(|brands| brands.join(", "))
                .unwrap_or_default();

            stmt.execute(params![
                interaction.drug_title,
                interaction.drug_substance,
                interaction.interacting_substance,
                interacting_brands,
                interaction.description,
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

/// Check a basket of brand-name drugs for interactions between them.
/// Uses three strategies:
/// 1. Exact substance match from pre-computed interactions table
/// 2. Full-text search of one drug's interaction text for the other's substance names
/// 3. ATC class-based matching for drug-class interactions (NSAR ↔ Antikoagulantien etc.)
fn demo_basket_check(db_path: &str, basket: &[&str]) -> Result<()> {
    let conn = Connection::open(db_path)?;

    let mut basket_drugs: Vec<BasketDrug> = Vec::new();
    for brand in basket {
        let mut stmt = conn.prepare(
            "SELECT brand_name, active_substances, atc_code, atc_class, interactions_text FROM drugs WHERE brand_name LIKE ?1",
        )?;
        let pattern = format!("%{}%", brand);
        let mut rows = stmt.query(params![pattern])?;
        if let Some(row) = rows.next()? {
            let brand_name: String = row.get(0)?;
            let substances_str: String = row.get(1)?;
            let atc_code: String = row.get::<_, Option<String>>(2)?.unwrap_or_default();
            let atc_class: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
            let interactions_text: String = row.get::<_, Option<String>>(4)?.unwrap_or_default();

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
                    "SELECT description FROM interactions WHERE drug_brand = ?1 AND interacting_substance = ?2",
                )?;
                let descs: Vec<String> = stmt
                    .query_map(params![a.brand, subst], |row| row.get::<_, String>(0))?
                    .filter_map(|r| r.ok())
                    .collect();
                for desc in &descs {
                    println!(
                        "\nINTERACTION [substance match]: {} <-> {}",
                        a.brand, b.brand
                    );
                    println!("  Via substance: {}", subst);
                    println!("  {}", desc);
                    found_any = true;
                }
            }

            // Reverse: B's text mentions A's substance
            for subst in &a.substances {
                let mut stmt = conn.prepare(
                    "SELECT description FROM interactions WHERE drug_brand = ?1 AND interacting_substance = ?2",
                )?;
                let descs: Vec<String> = stmt
                    .query_map(params![b.brand, subst], |row| row.get::<_, String>(0))?
                    .filter_map(|r| r.ok())
                    .collect();
                for desc in &descs {
                    println!(
                        "\nINTERACTION [substance match]: {} <-> {}",
                        b.brand, a.brand
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
                println!(
                    "\nINTERACTION [class-level]: {} <-> {} ({})",
                    a.brand, b.brand, hit.class_keyword
                );
                println!("  {}", hit.context);
                found_any = true;
            }

            let class_hits_ba = find_class_interactions(&b.interactions_text, &a.atc_code);
            for hit in &class_hits_ba {
                println!(
                    "\nINTERACTION [class-level]: {} <-> {} ({})",
                    b.brand, a.brand, hit.class_keyword
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
        ("B01A", &["antikoagul", "warfarin", "cumarin", "coumarin", "vitamin-k-antagonist", "vitamin k antagonist"]),
        // M01A = Nichtsteroidale Antiphlogistika (NSAIDs/NSAR)
        ("M01A", &["nsar", "nsaid", "nichtsteroidale antiphlogistika", "antiphlogistika", "nichtsteroidale antirheumatika"]),
        // N02B = Andere Analgetika und Antipyretika (incl. ASS)
        ("N02B", &["analgetik", "antipyretik", "acetylsalicylsäure"]),
        // C09A/C09B = ACE-Hemmer
        ("C09A", &["ace-hemmer", "ace-inhibitor", "ace inhibitor"]),
        ("C09B", &["ace-hemmer", "ace-inhibitor"]),
        // C09C/C09D = Angiotensin-II-Antagonisten
        ("C09C", &["angiotensin", "sartan"]),
        ("C09D", &["angiotensin", "sartan"]),
        // C07 = Beta-Blocker
        ("C07", &["beta-blocker", "betablocker", "β-blocker"]),
        // C03 = Diuretika
        ("C03", &["diuretik"]),
        // N06AB = SSRIs
        ("N06AB", &["ssri", "serotonin-wiederaufnahme", "serotonin reuptake"]),
        // A10 = Antidiabetika
        ("A10", &["antidiabetik", "insulin", "blutzucker"]),
        // H02 = Corticosteroide
        ("H02", &["corticosteroid", "kortikosteroid", "glucocorticoid"]),
        // L04 = Immunsuppressiva
        ("L04", &["immunsuppress", "ciclosporin", "tacrolimus"]),
        // L01 = Antineoplastische Mittel
        ("L01", &["antineoplast", "zytostatik", "methotrexat"]),
        // N03 = Antiepileptika
        ("N03", &["antiepileptik", "antikonvulsiv"]),
        // N05A = Antipsychotika
        ("N05A", &["antipsychoti", "neuroleptik"]),
        // N05B/N05C = Anxiolytika / Sedativa
        ("N05B", &["anxiolytik", "benzodiazepin"]),
        ("N05C", &["sedativ", "hypnotik", "schlafmittel"]),
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
