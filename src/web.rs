use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::{find_class_interactions, find_cyp_interactions, score_severity, severity_indicator};

struct AppState {
    db_path: String,
}

pub async fn serve(db_path: &str, port: u16) -> Result<()> {
    let state = Arc::new(AppState {
        db_path: db_path.to_string(),
    });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/api/search-drugs", get(search_drugs))
        .route("/api/check", post(check_interactions))
        .route("/api/search-interactions", get(search_interactions_api))
        .route("/api/class-interactions", get(class_interactions_api))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", port);
    println!("SDIF Web UI running at http://localhost:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

// --- Drug search / autocomplete ---

#[derive(Deserialize)]
struct DrugSearchQuery {
    q: String,
}

#[derive(Serialize)]
struct DrugResult {
    brand_name: String,
    atc_code: String,
    substances: String,
}

async fn search_drugs(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DrugSearchQuery>,
) -> impl IntoResponse {
    let q = query.q.trim().to_string();
    if q.len() < 2 {
        return Json(Vec::<DrugResult>::new()).into_response();
    }

    let db_path = state.db_path.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<DrugResult>> {
        let conn = Connection::open(&db_path)?;
        let pattern = format!("%{}%", q);
        let mut stmt = conn.prepare(
            "SELECT DISTINCT brand_name, atc_code, active_substances FROM drugs \
             WHERE brand_name LIKE ?1 OR active_substances LIKE ?1 \
             ORDER BY brand_name LIMIT 20",
        )?;
        let rows = stmt
            .query_map(params![pattern], |row| {
                Ok(DrugResult {
                    brand_name: row.get(0)?,
                    atc_code: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    substances: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(rows)
    })
    .await;

    match result {
        Ok(Ok(drugs)) => Json(drugs).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// --- Basket interaction check ---

#[derive(Deserialize)]
struct CheckRequest {
    drugs: Vec<String>,
}

#[derive(Serialize)]
struct CheckResponse {
    basket: Vec<BasketDrugInfo>,
    interactions: Vec<InteractionResult>,
}

#[derive(Serialize)]
struct BasketDrugInfo {
    brand: String,
    atc_code: String,
    substances: Vec<String>,
}

#[derive(Serialize)]
struct InteractionResult {
    drug_a: String,
    drug_b: String,
    interaction_type: String, // "substance", "class-level", "CYP"
    severity_score: u8,
    severity_label: String,
    severity_indicator: String,
    keyword: String,
    description: String,
}

struct BasketDrug {
    brand: String,
    substances: Vec<String>,
    atc_code: String,
    interactions_text: String,
}

fn resolve_drug(conn: &Connection, input: &str) -> Option<BasketDrug> {
    let pattern = format!("%{}%", input);
    // Try brand name first
    let mut stmt = conn
        .prepare(
            "SELECT brand_name, active_substances, atc_code, interactions_text \
             FROM drugs WHERE brand_name LIKE ?1 ORDER BY length(interactions_text) DESC",
        )
        .ok()?;
    let mut rows = stmt.query(params![pattern]).ok()?;
    if let Some(row) = rows.next().ok()? {
        let brand: String = row.get(0).ok()?;
        let substances_str: String = row.get(1).ok()?;
        let atc_code: String = row.get::<_, Option<String>>(2).ok()?.unwrap_or_default();
        let interactions_text: String = row.get::<_, Option<String>>(3).ok()?.unwrap_or_default();
        let substances: Vec<String> = substances_str
            .split(", ")
            .map(|s| s.to_lowercase())
            .collect();
        return Some(BasketDrug {
            brand,
            substances,
            atc_code,
            interactions_text,
        });
    }
    drop(rows);
    drop(stmt);
    // Try substance name
    let mut stmt2 = conn
        .prepare(
            "SELECT DISTINCT d.brand_name, d.active_substances, d.atc_code, d.interactions_text \
             FROM substance_brand_map s JOIN drugs d ON d.brand_name = s.brand_name \
             WHERE s.substance LIKE ?1 ORDER BY length(d.interactions_text) DESC LIMIT 1",
        )
        .ok()?;
    let pattern_lower = format!("%{}%", input.to_lowercase());
    let mut rows2 = stmt2.query(params![pattern_lower]).ok()?;
    if let Some(row) = rows2.next().ok()? {
        let brand: String = row.get(0).ok()?;
        let substances_str: String = row.get(1).ok()?;
        let atc_code: String = row.get::<_, Option<String>>(2).ok()?.unwrap_or_default();
        let interactions_text: String = row.get::<_, Option<String>>(3).ok()?.unwrap_or_default();
        let substances: Vec<String> = substances_str
            .split(", ")
            .map(|s| s.to_lowercase())
            .collect();
        return Some(BasketDrug {
            brand,
            substances,
            atc_code,
            interactions_text,
        });
    }
    None
}

async fn check_interactions(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CheckRequest>,
) -> impl IntoResponse {
    let db_path = state.db_path.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<CheckResponse> {
        let conn = Connection::open(&db_path)?;

        let mut basket_drugs: Vec<BasketDrug> = Vec::new();
        for input in &req.drugs {
            if let Some(drug) = resolve_drug(&conn, input) {
                basket_drugs.push(drug);
            }
        }

        let basket: Vec<BasketDrugInfo> = basket_drugs
            .iter()
            .map(|d| BasketDrugInfo {
                brand: d.brand.clone(),
                atc_code: d.atc_code.clone(),
                substances: d.substances.clone(),
            })
            .collect();

        let mut interactions = Vec::new();

        for i in 0..basket_drugs.len() {
            for j in (i + 1)..basket_drugs.len() {
                let a = &basket_drugs[i];
                let b = &basket_drugs[j];

                // Strategy 1: Substance match A->B
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
                    for (desc, sev_score, sev_label) in rows {
                        interactions.push(InteractionResult {
                            drug_a: a.brand.clone(),
                            drug_b: b.brand.clone(),
                            interaction_type: "substance".to_string(),
                            severity_score: sev_score,
                            severity_label: sev_label,
                            severity_indicator: severity_indicator(sev_score).to_string(),
                            keyword: subst.clone(),
                            description: desc,
                        });
                    }
                }

                // Substance match B->A
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
                    for (desc, sev_score, sev_label) in rows {
                        interactions.push(InteractionResult {
                            drug_a: b.brand.clone(),
                            drug_b: a.brand.clone(),
                            interaction_type: "substance".to_string(),
                            severity_score: sev_score,
                            severity_label: sev_label,
                            severity_indicator: severity_indicator(sev_score).to_string(),
                            keyword: subst.clone(),
                            description: desc,
                        });
                    }
                }

                // Strategy 2: Class-level
                for hit in find_class_interactions(&a.interactions_text, &b.atc_code) {
                    let (sev_score, sev_label) = score_severity(&hit.context);
                    interactions.push(InteractionResult {
                        drug_a: a.brand.clone(),
                        drug_b: b.brand.clone(),
                        interaction_type: "class-level".to_string(),
                        severity_score: sev_score,
                        severity_label: sev_label.to_string(),
                        severity_indicator: severity_indicator(sev_score).to_string(),
                        keyword: hit.class_keyword,
                        description: hit.context,
                    });
                }
                for hit in find_class_interactions(&b.interactions_text, &a.atc_code) {
                    let (sev_score, sev_label) = score_severity(&hit.context);
                    interactions.push(InteractionResult {
                        drug_a: b.brand.clone(),
                        drug_b: a.brand.clone(),
                        interaction_type: "class-level".to_string(),
                        severity_score: sev_score,
                        severity_label: sev_label.to_string(),
                        severity_indicator: severity_indicator(sev_score).to_string(),
                        keyword: hit.class_keyword,
                        description: hit.context,
                    });
                }

                // Strategy 3: CYP
                for hit in find_cyp_interactions(&a.interactions_text, &b.atc_code, &b.substances) {
                    let (sev_score, sev_label) = score_severity(&hit.context);
                    interactions.push(InteractionResult {
                        drug_a: a.brand.clone(),
                        drug_b: b.brand.clone(),
                        interaction_type: "CYP".to_string(),
                        severity_score: sev_score,
                        severity_label: sev_label.to_string(),
                        severity_indicator: severity_indicator(sev_score).to_string(),
                        keyword: hit.class_keyword,
                        description: hit.context,
                    });
                }
                for hit in find_cyp_interactions(&b.interactions_text, &a.atc_code, &a.substances) {
                    let (sev_score, sev_label) = score_severity(&hit.context);
                    interactions.push(InteractionResult {
                        drug_a: b.brand.clone(),
                        drug_b: a.brand.clone(),
                        interaction_type: "CYP".to_string(),
                        severity_score: sev_score,
                        severity_label: sev_label.to_string(),
                        severity_indicator: severity_indicator(sev_score).to_string(),
                        keyword: hit.class_keyword,
                        description: hit.context,
                    });
                }
            }
        }

        // Sort by severity descending
        interactions.sort_by(|a, b| b.severity_score.cmp(&a.severity_score));

        Ok(CheckResponse {
            basket,
            interactions,
        })
    })
    .await;

    match result {
        Ok(Ok(resp)) => Json(resp).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// --- Search interactions ---

#[derive(Deserialize)]
struct SearchQuery {
    term: String,
    limit: Option<usize>,
}

#[derive(Serialize)]
struct SearchResult {
    drug_brand: String,
    interacting_substance: String,
    interacting_brand: String,
    severity_score: u8,
    severity_label: String,
    severity_indicator: String,
    description: String,
}

#[derive(Serialize)]
struct SearchResponse {
    total: usize,
    results: Vec<SearchResult>,
}

async fn search_interactions_api(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SearchQuery>,
) -> impl IntoResponse {
    let db_path = state.db_path.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<SearchResponse> {
        let conn = Connection::open(&db_path)?;
        let pattern = format!("%{}%", query.term);

        let total: usize = conn.query_row(
            "SELECT COUNT(*) FROM interactions WHERE description LIKE ?1",
            params![pattern],
            |row| row.get(0),
        )?;

        let sql = if query.limit.is_some() {
            "SELECT drug_brand, drug_substance, interacting_substance, interacting_brands, description, severity_score, severity_label \
             FROM interactions WHERE description LIKE ?1 ORDER BY severity_score DESC LIMIT ?2"
        } else {
            "SELECT drug_brand, drug_substance, interacting_substance, interacting_brands, description, severity_score, severity_label \
             FROM interactions WHERE description LIKE ?1 ORDER BY severity_score DESC"
        };

        let mut stmt = conn.prepare(sql)?;
        let rows: Vec<SearchResult> = if let Some(lim) = query.limit {
            stmt.query_map(params![pattern, lim], |row| {
                let interacting_brands: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
                let first_brand = interacting_brands.split(", ").next().unwrap_or("").to_string();
                Ok(SearchResult {
                    drug_brand: row.get(0)?,
                    interacting_substance: row.get(2)?,
                    interacting_brand: first_brand,
                    severity_score: row.get(5)?,
                    severity_label: row.get(6)?,
                    severity_indicator: severity_indicator(row.get::<_, u8>(5)?).to_string(),
                    description: row.get(4)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect()
        } else {
            stmt.query_map(params![pattern], |row| {
                let interacting_brands: String = row.get::<_, Option<String>>(3)?.unwrap_or_default();
                let first_brand = interacting_brands.split(", ").next().unwrap_or("").to_string();
                Ok(SearchResult {
                    drug_brand: row.get(0)?,
                    interacting_substance: row.get(2)?,
                    interacting_brand: first_brand,
                    severity_score: row.get(5)?,
                    severity_label: row.get(6)?,
                    severity_indicator: severity_indicator(row.get::<_, u8>(5)?).to_string(),
                    description: row.get(4)?,
                })
            })?
            .filter_map(|r| r.ok())
            .collect()
        };

        Ok(SearchResponse {
            total,
            results: rows,
        })
    })
    .await;

    match result {
        Ok(Ok(resp)) => Json(resp).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// --- Class-level interaction overview ---

#[derive(Serialize)]
struct ClassInteractionRow {
    atc_prefix: String,
    description: String,
    drugs_in_class: usize,
    drugs_mentioning: usize,
    potential_pairs: u64,
    top_keyword: String,
}

#[derive(Serialize)]
struct ClassInteractionsResponse {
    total_pairs: u64,
    classes: Vec<ClassInteractionRow>,
}

fn atc_class_description(prefix: &str) -> &'static str {
    match prefix {
        "B01A" => "Antikoagulantien",
        "B01AC" => "Thrombozytenaggregationshemmer",
        "M01A" => "NSAR (NSAIDs)",
        "N02B" => "Analgetika / Antipyretika",
        "N02A" => "Opioide",
        "C09A" => "ACE-Hemmer",
        "C09B" => "ACE-Hemmer (Kombination)",
        "C09C" => "Sartane (AT1-Antagonisten)",
        "C09D" => "Sartane (Kombination)",
        "C07" => "Beta-Blocker",
        "C08" => "Calciumkanalblocker",
        "C03" => "Diuretika",
        "C03C" => "Schleifendiuretika",
        "C03A" => "Thiazide",
        "C01A" => "Herzglykoside",
        "C01B" => "Antiarrhythmika",
        "C10A" => "Statine",
        "N06AB" => "SSRIs",
        "N06A" => "Antidepressiva",
        "A10" => "Antidiabetika",
        "H02" => "Corticosteroide",
        "L04" => "Immunsuppressiva",
        "L01" => "Antineoplastika",
        "N03" => "Antiepileptika",
        "N05A" => "Antipsychotika",
        "N05B" => "Anxiolytika",
        "N05C" => "Sedativa / Hypnotika",
        "J01" => "Antibiotika",
        "J01FA" => "Makrolide",
        "J01MA" => "Fluorchinolone",
        "J02A" => "Antimykotika",
        "J05A" => "Antivirale",
        "A02BC" => "PPI (Protonenpumpenhemmer)",
        "A02B" => "Ulkusmittel",
        "G03A" => "Hormonale Kontrazeptiva",
        "N07" => "Nervensystem (andere)",
        "R03" => "Bronchodilatatoren",
        "M04" => "Gichtmittel",
        "B03" => "Eisenpräparate",
        "L02BA" => "SERMs (Tamoxifen)",
        "L02B" => "Hormonantagonisten",
        "V03AB" => "Antidota",
        "M03A" => "Muskelrelaxantien",
        _ => "",
    }
}

async fn class_interactions_api(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let db_path = state.db_path.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<ClassInteractionsResponse> {
        let conn = Connection::open(&db_path)?;

        let mut stmt = conn.prepare(
            "SELECT brand_name, atc_code, active_substances, interactions_text FROM drugs \
             WHERE length(interactions_text) > 0 AND atc_code IS NOT NULL AND atc_code != ''"
        )?;
        struct DrugRow { atc: String, substances: String, text: String }
        let drugs: Vec<DrugRow> = stmt
            .query_map([], |row| {
                Ok(DrugRow {
                    atc: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    substances: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    text: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                })
            })?
            .filter_map(|r| r.ok())
            .filter(|d| !d.atc.is_empty() && !d.text.is_empty())
            .collect();

        let class_keywords: Vec<(&str, Vec<&str>)> = vec![
            ("B01A", vec!["antikoagul", "warfarin", "cumarin", "coumarin", "vitamin-k-antagonist",
                        "vitamin k antagonist", "blutgerinnungshemm", "thrombozytenaggregationshemm",
                        "plättchenhemm", "antithrombotisch", "heparin", "thrombin-hemm",
                        "faktor-xa", "direktes orales antikoagulans", "doak"]),
            ("B01AC", vec!["thrombozytenaggregationshemm", "plättchenhemm", "thrombocytenaggregation"]),
            ("M01A", vec!["nsar", "nsaid", "nichtsteroidale antiphlogistika", "antiphlogistika",
                        "nichtsteroidale antirheumatika", "cox-2", "cox-hemmer", "cyclooxygenase",
                        "prostaglandinsynthesehemm", "entzündungshemm"]),
            ("N02B", vec!["analgetik", "antipyretik", "acetylsalicylsäure", "paracetamol"]),
            ("N02A", vec!["opioid", "opiat", "morphin", "atemdepression", "zns-depression"]),
            ("C09A", vec!["ace-hemmer", "ace-inhibitor", "ace inhibitor", "angiotensin-converting"]),
            ("C09B", vec!["ace-hemmer", "ace-inhibitor", "angiotensin-converting"]),
            ("C09C", vec!["angiotensin", "sartan", "at1-rezeptor", "at1-antagonist", "at1-blocker"]),
            ("C09D", vec!["angiotensin", "sartan", "at1-rezeptor", "at1-antagonist"]),
            ("C07", vec!["beta-blocker", "betablocker", "\u{03b2}-blocker", "betarezeptorenblocker", "beta-adrenozeptor"]),
            ("C08", vec!["calciumantagonist", "calciumkanalblocker", "kalziumantagonist", "kalziumkanalblocker", "calcium-antagonist"]),
            ("C03", vec!["diuretik", "thiazid", "schleifendiuretik", "kaliumsparend"]),
            ("C03C", vec!["schleifendiuretik", "furosemid", "torasemid"]),
            ("C03A", vec!["thiazid", "hydrochlorothiazid"]),
            ("C01A", vec!["herzglykosid", "digoxin", "digitalis", "digitoxin"]),
            ("C01B", vec!["antiarrhythmi", "amiodaron"]),
            ("C10A", vec!["statin", "hmg-coa", "lipidsenk", "cholesterinsenk"]),
            ("N06AB", vec!["ssri", "serotonin-wiederaufnahme", "serotonin reuptake", "selektive serotonin", "serotonerg"]),
            ("N06A", vec!["antidepressiv", "trizyklisch", "serotonin", "snri", "maoh", "mao-hemmer", "monoaminoxidase"]),
            ("A10", vec!["antidiabetik", "insulin", "blutzucker", "hypoglyk\u{00e4}mie", "orale antidiabetika", "sulfonylharnstoff", "metformin"]),
            ("H02", vec!["corticosteroid", "kortikosteroid", "glucocorticoid", "glukokortikoid", "kortison", "steroid"]),
            ("L04", vec!["immunsuppress", "ciclosporin", "tacrolimus", "mycophenolat", "azathioprin", "sirolimus"]),
            ("L01", vec!["antineoplast", "zytostatik", "methotrexat", "chemotherap"]),
            ("N03", vec!["antiepileptik", "antikonvulsiv", "krampfl\u{00f6}send", "carbamazepin", "valproins\u{00e4}ure", "phenytoin", "enzymindukt"]),
            ("N05A", vec!["antipsychoti", "neuroleptik", "qt-verl\u{00e4}nger", "qt-zeit"]),
            ("N05B", vec!["anxiolytik", "benzodiazepin"]),
            ("N05C", vec!["sedativ", "hypnotik", "schlafmittel", "zns-d\u{00e4}mfpend", "zns-depression"]),
            ("J01", vec!["antibiotik", "antibakteriell"]),
            ("J01FA", vec!["makrolid", "erythromycin", "clarithromycin", "azithromycin"]),
            ("J01MA", vec!["fluorchinolon", "chinolon", "gyrasehemm"]),
            ("J02A", vec!["antimykotik", "azol-antimykotik", "triazol", "itraconazol", "fluconazol", "voriconazol", "cyp3a4-hemm"]),
            ("J05A", vec!["antiviral", "proteasehemm", "protease-inhibitor", "hiv"]),
            ("A02BC", vec!["protonenpumpeninhibitor", "protonenpumpenhemm", "ppi", "s\u{00e4}ureblocker"]),
            ("A02B", vec!["antazid", "h2-blocker", "h2-antagonist", "s\u{00e4}urehemm"]),
            ("G03A", vec!["kontrazeptiv", "\u{00f6}strogen", "orale kontrazeptiva", "hormonelle verh\u{00fc}tung"]),
            ("N07", vec!["dopaminerg", "cholinerg", "anticholinerg"]),
            ("R03", vec!["bronchodilatat", "theophyllin", "sympathomimetik", "beta-2"]),
            ("M04", vec!["urikosurik", "gichtmittel", "harns\u{00e4}ure", "allopurinol"]),
            ("B03", vec!["eisen", "eisenpr\u{00e4}parat", "eisensupplementation"]),
            ("L02BA", vec!["toremifen", "tamoxifen", "anti\u{00f6}strogen", "\u{00f6}strogen-rezeptor", "serm", "selektive \u{00f6}strogenrezeptor"]),
            ("L02B", vec!["hormonantagonist", "antihormon", "antiandrogen", "anti\u{00f6}strogen"]),
            ("V03AB", vec!["sugammadex", "antidot", "antagonisierung", "neuromuskul\u{00e4}re blockade", "verdr\u{00e4}ngung"]),
            ("M03A", vec!["muskelrelax", "neuromuskul\u{00e4}r", "rocuronium", "vecuronium", "succinylcholin", "curare"]),
        ];

        let mut drugs_in_class: HashMap<String, usize> = HashMap::new();
        for (prefix, _) in &class_keywords {
            let count = drugs.iter().filter(|d| d.atc.starts_with(prefix)).count();
            drugs_in_class.insert(prefix.to_string(), count);
        }

        let mut total_pairs = 0u64;
        let mut classes = Vec::new();

        for (prefix, keywords) in &class_keywords {
            let n_in_class = *drugs_in_class.get(*prefix).unwrap_or(&0);
            if n_in_class == 0 { continue; }

            let mut mentioning_substances: HashSet<String> = HashSet::new();
            let mut best_keyword = String::new();
            let mut best_count = 0usize;

            for kw in keywords {
                let mut count = 0usize;
                for drug in &drugs {
                    if drug.atc.starts_with(prefix) { continue; }
                    let text_lower = drug.text.to_lowercase();
                    if text_lower.contains(kw) {
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
            let pair_count = (n_mentioning as u64) * (n_in_class as u64);
            total_pairs += pair_count;
            classes.push(ClassInteractionRow {
                atc_prefix: prefix.to_string(),
                description: atc_class_description(prefix).to_string(),
                drugs_in_class: n_in_class,
                drugs_mentioning: n_mentioning,
                potential_pairs: pair_count,
                top_keyword: best_keyword,
            });
        }

        classes.sort_by(|a, b| b.potential_pairs.cmp(&a.potential_pairs));

        Ok(ClassInteractionsResponse { total_pairs, classes })
    })
    .await;

    match result {
        Ok(Ok(resp)) => Json(resp).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
