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

use crate::{find_class_interactions, find_cyp_interactions, load_class_keywords, load_cyp_rules, score_severity, severity_indicator};

struct AppState {
    db_path: String,
    epha: bool,
}

pub async fn serve(db_path: &str, port: u16, epha: bool) -> Result<()> {
    if epha {
        println!("EPha interactions enabled");
    }
    let state = Arc::new(AppState {
        db_path: db_path.to_string(),
        epha,
    });

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/api/search-drugs", get(search_drugs))
        .route("/api/check", post(check_interactions))
        .route("/api/search-interactions", get(search_interactions_api))
        .route("/api/suggest-terms", get(suggest_terms_api))
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
    q: Option<String>,
    atc: Option<String>,
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
    let db_path = state.db_path.clone();

    // Exact ATC code lookup
    if let Some(atc) = &query.atc {
        let atc = atc.trim().to_string();
        if atc.is_empty() {
            return Json(Vec::<DrugResult>::new()).into_response();
        }
        let result = tokio::task::spawn_blocking(move || -> Result<Vec<DrugResult>> {
            let conn = Connection::open(&db_path)?;
            let mut stmt = conn.prepare(
                "SELECT brand_name, atc_code, active_substances FROM drugs \
                 WHERE atc_code = ?1 ORDER BY length(interactions_text) DESC LIMIT 1",
            )?;
            let rows = stmt
                .query_map(params![atc], |row| {
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
        return match result {
            Ok(Ok(drugs)) => Json(drugs).into_response(),
            _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
    }

    let q = query.q.as_deref().unwrap_or("").trim().to_string();
    if q.len() < 2 {
        return Json(Vec::<DrugResult>::new()).into_response();
    }

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
    drug_a_atc: String,
    drug_b: String,
    drug_b_atc: String,
    interaction_type: String, // "substance", "class-level", "CYP", "epha"
    severity_score: u8,
    severity_label: String,
    severity_indicator: String,
    keyword: String,
    description: String,
    explanation: String,
    source: String, // "Swissmedic FI" or "EPha"
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
    let epha_enabled = state.epha;
    let fi_source = if epha_enabled { "Swissmedic FI".to_string() } else { String::new() };
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

        let class_keywords = load_class_keywords(&conn);
        let cyp_rules = load_cyp_rules(&conn);

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
                            drug_a_atc: a.atc_code.clone(),
                            drug_b: b.brand.clone(),
                            drug_b_atc: b.atc_code.clone(),
                            interaction_type: "substance".to_string(),
                            severity_score: sev_score,
                            severity_label: sev_label,
                            severity_indicator: severity_indicator(sev_score).to_string(),
                            keyword: subst.clone(),
                            description: desc,
                            explanation: format!("Wirkstoff «{}» wird in der Fachinformation von {} erwähnt", subst, a.brand),
                            source: fi_source.clone(),
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
                            drug_a_atc: b.atc_code.clone(),
                            drug_b: a.brand.clone(),
                            drug_b_atc: a.atc_code.clone(),
                            interaction_type: "substance".to_string(),
                            severity_score: sev_score,
                            severity_label: sev_label,
                            severity_indicator: severity_indicator(sev_score).to_string(),
                            keyword: subst.clone(),
                            description: desc,
                            explanation: format!("Wirkstoff «{}» wird in der Fachinformation von {} erwähnt", subst, b.brand),
                            source: fi_source.clone(),
                        });
                    }
                }

                // Strategy 2: Class-level
                for hit in find_class_interactions(&a.interactions_text, &b.atc_code, &class_keywords) {
                    let (sev_score, sev_label) = score_severity(&hit.context);
                    let class_desc = atc_class_description_for_code(&b.atc_code);
                    interactions.push(InteractionResult {
                        drug_a: a.brand.clone(),
                        drug_a_atc: a.atc_code.clone(),
                        drug_b: b.brand.clone(),
                        drug_b_atc: b.atc_code.clone(),
                        interaction_type: "class-level".to_string(),
                        severity_score: sev_score,
                        severity_label: sev_label.to_string(),
                        severity_indicator: severity_indicator(sev_score).to_string(),
                        keyword: hit.class_keyword.clone(),
                        description: hit.context,
                        explanation: format!("{} [{}] gehört zur Klasse {} — Keyword «{}» gefunden in Fachinformation von {}",
                            b.brand, b.atc_code, class_desc, hit.class_keyword, a.brand),
                        source: fi_source.clone(),
                    });
                }
                for hit in find_class_interactions(&b.interactions_text, &a.atc_code, &class_keywords) {
                    let (sev_score, sev_label) = score_severity(&hit.context);
                    let class_desc = atc_class_description_for_code(&a.atc_code);
                    interactions.push(InteractionResult {
                        drug_a: b.brand.clone(),
                        drug_a_atc: b.atc_code.clone(),
                        drug_b: a.brand.clone(),
                        drug_b_atc: a.atc_code.clone(),
                        interaction_type: "class-level".to_string(),
                        severity_score: sev_score,
                        severity_label: sev_label.to_string(),
                        severity_indicator: severity_indicator(sev_score).to_string(),
                        keyword: hit.class_keyword.clone(),
                        description: hit.context,
                        explanation: format!("{} [{}] gehört zur Klasse {} — Keyword «{}» gefunden in Fachinformation von {}",
                            a.brand, a.atc_code, class_desc, hit.class_keyword, b.brand),
                        source: fi_source.clone(),
                    });
                }

                // Strategy 3: CYP
                for hit in find_cyp_interactions(&a.interactions_text, &b.atc_code, &b.substances, &cyp_rules) {
                    let (sev_score, sev_label) = score_severity(&hit.context);
                    interactions.push(InteractionResult {
                        drug_a: a.brand.clone(),
                        drug_a_atc: a.atc_code.clone(),
                        drug_b: b.brand.clone(),
                        drug_b_atc: b.atc_code.clone(),
                        interaction_type: "CYP".to_string(),
                        severity_score: sev_score,
                        severity_label: sev_label.to_string(),
                        severity_indicator: severity_indicator(sev_score).to_string(),
                        keyword: hit.class_keyword.clone(),
                        description: hit.context,
                        explanation: format!("{} ist {} — Fachinformation von {} erwähnt dieses Enzym",
                            b.brand, hit.class_keyword, a.brand),
                        source: fi_source.clone(),
                    });
                }
                for hit in find_cyp_interactions(&b.interactions_text, &a.atc_code, &a.substances, &cyp_rules) {
                    let (sev_score, sev_label) = score_severity(&hit.context);
                    interactions.push(InteractionResult {
                        drug_a: b.brand.clone(),
                        drug_a_atc: b.atc_code.clone(),
                        drug_b: a.brand.clone(),
                        drug_b_atc: a.atc_code.clone(),
                        interaction_type: "CYP".to_string(),
                        severity_score: sev_score,
                        severity_label: sev_label.to_string(),
                        severity_indicator: severity_indicator(sev_score).to_string(),
                        keyword: hit.class_keyword.clone(),
                        description: hit.context,
                        explanation: format!("{} ist {} — Fachinformation von {} erwähnt dieses Enzym",
                            a.brand, hit.class_keyword, b.brand),
                        source: fi_source.clone(),
                    });
                }

                // Strategy 4: EPha curated interactions by ATC pair
                if epha_enabled {
                let mut epha_stmt = conn.prepare(
                    "SELECT risk_class, risk_label, effect, mechanism, measures, severity_score, title \
                     FROM epha_interactions WHERE (atc1 = ?1 AND atc2 = ?2) OR (atc1 = ?2 AND atc2 = ?1) LIMIT 1",
                )?;
                let epha_rows: Vec<(String, String, String, String, String, u8, String)> = epha_stmt
                    .query_map(params![a.atc_code, b.atc_code], |row| {
                        Ok((
                            row.get(0)?, row.get(1)?, row.get(2)?,
                            row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?,
                        ))
                    })?
                    .filter_map(|r| r.ok())
                    .collect();
                for (risk_class, risk_label, effect, mechanism, measures, sev_score, _title) in epha_rows {
                    let desc = if mechanism.is_empty() {
                        effect.clone()
                    } else {
                        format!("{}\n\nMechanismus: {}\n\nMassnahmen: {}", effect, mechanism, measures)
                    };
                    interactions.push(InteractionResult {
                        drug_a: a.brand.clone(),
                        drug_a_atc: a.atc_code.clone(),
                        drug_b: b.brand.clone(),
                        drug_b_atc: b.atc_code.clone(),
                        interaction_type: "epha".to_string(),
                        severity_score: sev_score,
                        severity_label: risk_label,
                        severity_indicator: severity_indicator(sev_score).to_string(),
                        keyword: risk_class,
                        description: desc,
                        explanation: format!("EPha Interaktionsdatenbank (ATC {} ↔ {})", a.atc_code, b.atc_code),
                        source: "EPha".to_string(),
                    });
                }
                } // end if epha_enabled
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
    source: String,
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
    let epha_enabled = state.epha;
    let fi_source = if epha_enabled { "Swissmedic FI".to_string() } else { String::new() };
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
                    source: fi_source.clone(),
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
                    source: fi_source.clone(),
                })
            })?
            .filter_map(|r| r.ok())
            .collect()
        };

        // Also search EPha interactions (if enabled)
        let (epha_rows, epha_total) = if epha_enabled {
            let epha_sql = if query.limit.is_some() {
                "SELECT title, effect, mechanism, measures, risk_class, risk_label, severity_score \
                 FROM epha_interactions WHERE effect LIKE ?1 OR mechanism LIKE ?1 OR measures LIKE ?1 \
                 ORDER BY severity_score DESC LIMIT ?2"
            } else {
                "SELECT title, effect, mechanism, measures, risk_class, risk_label, severity_score \
                 FROM epha_interactions WHERE effect LIKE ?1 OR mechanism LIKE ?1 OR measures LIKE ?1 \
                 ORDER BY severity_score DESC"
            };
            let mut epha_stmt = conn.prepare(epha_sql)?;
            let er: Vec<SearchResult> = if let Some(lim) = query.limit {
                epha_stmt.query_map(params![pattern, lim], |row| {
                    let effect: String = row.get(1)?;
                    let mechanism: String = row.get(2)?;
                    let measures: String = row.get(3)?;
                    let desc = if mechanism.is_empty() {
                        effect
                    } else {
                        format!("{}\n\nMechanismus: {}\n\nMassnahmen: {}", effect, mechanism, measures)
                    };
                    Ok(SearchResult {
                        drug_brand: row.get(0)?,
                        interacting_substance: String::new(),
                        interacting_brand: String::new(),
                        severity_score: row.get(6)?,
                        severity_label: row.get(5)?,
                        severity_indicator: severity_indicator(row.get::<_, u8>(6)?).to_string(),
                        description: desc,
                        source: "EPha".to_string(),
                    })
                })?.filter_map(|r| r.ok()).collect()
            } else {
                epha_stmt.query_map(params![pattern], |row| {
                    let effect: String = row.get(1)?;
                    let mechanism: String = row.get(2)?;
                    let measures: String = row.get(3)?;
                    let desc = if mechanism.is_empty() {
                        effect
                    } else {
                        format!("{}\n\nMechanismus: {}\n\nMassnahmen: {}", effect, mechanism, measures)
                    };
                    Ok(SearchResult {
                        drug_brand: row.get(0)?,
                        interacting_substance: String::new(),
                        interacting_brand: String::new(),
                        severity_score: row.get(6)?,
                        severity_label: row.get(5)?,
                        severity_indicator: severity_indicator(row.get::<_, u8>(6)?).to_string(),
                        description: desc,
                        source: "EPha".to_string(),
                    })
                })?.filter_map(|r| r.ok()).collect()
            };
            let et: usize = conn.query_row(
                "SELECT COUNT(*) FROM epha_interactions WHERE effect LIKE ?1 OR mechanism LIKE ?1 OR measures LIKE ?1",
                params![pattern],
                |row| row.get(0),
            )?;
            (er, et)
        } else {
            (Vec::new(), 0)
        };

        let mut all_results = rows;
        all_results.extend(epha_rows);
        all_results.sort_by(|a, b| b.severity_score.cmp(&a.severity_score));

        Ok(SearchResponse {
            total: total + epha_total,
            results: all_results,
        })
    })
    .await;

    match result {
        Ok(Ok(resp)) => Json(resp).into_response(),
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

// --- Suggest clinical search terms ---

#[derive(Deserialize)]
struct SuggestQuery {
    q: String,
}

#[derive(Serialize)]
struct TermSuggestion {
    term: String,
    count: usize,
}

async fn suggest_terms_api(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SuggestQuery>,
) -> impl IntoResponse {
    let q = query.q.trim().to_lowercase();
    if q.len() < 2 {
        return Json(Vec::<TermSuggestion>::new()).into_response();
    }

    let db_path = state.db_path.clone();
    let epha_enabled = state.epha;
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<TermSuggestion>> {
        let conn = Connection::open(&db_path)?;
        let pattern = format!("%{}%", q);
        let mut stmt = conn.prepare(
            "SELECT description FROM interactions WHERE description LIKE ?1 LIMIT 500"
        )?;
        let mut descriptions: Vec<String> = stmt
            .query_map(params![pattern], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        // Also include EPha descriptions (if enabled)
        if epha_enabled {
        let mut epha_stmt = conn.prepare(
            "SELECT effect || ' ' || mechanism || ' ' || measures FROM epha_interactions \
             WHERE effect LIKE ?1 OR mechanism LIKE ?1 OR measures LIKE ?1 LIMIT 500"
        )?;
        let epha_descs: Vec<String> = epha_stmt
            .query_map(params![pattern], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        descriptions.extend(epha_descs);
        }

        // Extract words/phrases around the matching term
        // term_counts: lowercase key -> total count
        // form_counts: lowercase key -> (original form -> count) to pick best capitalization
        let mut term_counts: HashMap<String, usize> = HashMap::new();
        let mut form_counts: HashMap<String, HashMap<String, usize>> = HashMap::new();
        let q_lower = q.to_lowercase();

        let mut add_term = |original: &str| {
            let key = original.to_lowercase();
            *term_counts.entry(key.clone()).or_insert(0) += 1;
            *form_counts.entry(key).or_default().entry(original.to_string()).or_insert(0) += 1;
        };

        for desc in &descriptions {
            let desc_lower = desc.to_lowercase();
            let mut pos = 0;
            while let Some(idx) = desc_lower[pos..].find(&q_lower) {
                let abs_idx = pos + idx;
                // Expand to word boundaries (include hyphenated/compound words)
                let start = desc_lower[..abs_idx]
                    .rfind(|c: char| c.is_whitespace() || c == '(' || c == ')')
                    .map(|i| i + 1)
                    .unwrap_or(0);
                let end = desc_lower[abs_idx..]
                    .find(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',' || c == '.' || c == ';')
                    .map(|i| abs_idx + i)
                    .unwrap_or(desc_lower.len());

                let word = desc[start..end].trim();
                if word.len() >= q.len() + 1 && word.len() <= 40 {
                    add_term(word);
                }

                // Also extract two-word phrases (bigrams)
                // Look for the next word after the current match
                let after = &desc_lower[end..];
                if let Some(next_start_offset) = after.find(|c: char| !c.is_whitespace() && c != ',' && c != ';' && c != '.' && c != ')') {
                    let next_abs_start = end + next_start_offset;
                    // Only proceed if the separator was whitespace (not punctuation)
                    let separator = &desc_lower[end..next_abs_start];
                    if separator.chars().all(|c| c.is_whitespace()) && !separator.is_empty() {
                        let next_end = desc_lower[next_abs_start..]
                            .find(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == ',' || c == '.' || c == ';')
                            .map(|i| next_abs_start + i)
                            .unwrap_or(desc_lower.len());
                        let bigram = desc[start..next_end].trim();
                        if bigram.len() > word.len() + 1 && bigram.len() <= 60 {
                            add_term(bigram);
                        }
                    }
                }

                // Also look for the previous word before the current match
                let before = &desc_lower[..start];
                if let Some(prev_end_offset) = before.rfind(|c: char| !c.is_whitespace()) {
                    let prev_end = prev_end_offset + 1;
                    let prev_char = before.as_bytes()[prev_end_offset];
                    // Only if previous char is a letter (not punctuation)
                    if (prev_char as char).is_alphanumeric() {
                        let prev_start = desc_lower[..prev_end]
                            .rfind(|c: char| c.is_whitespace() || c == '(' || c == ')')
                            .map(|i| i + 1)
                            .unwrap_or(0);
                        let bigram = desc[prev_start..end].trim();
                        if bigram.len() > word.len() + 1 && bigram.len() <= 60 {
                            add_term(bigram);
                        }
                    }
                }

                pos = abs_idx + q_lower.len();
            }
        }

        let mut suggestions: Vec<TermSuggestion> = term_counts
            .into_iter()
            .filter(|(_, count)| *count >= 2)
            .map(|(key, count)| {
                // Pick the most frequent original-case form
                let display = form_counts.get(&key)
                    .and_then(|forms| forms.iter().max_by_key(|(_, c)| *c).map(|(f, _)| f.clone()))
                    .unwrap_or(key);
                TermSuggestion { term: display, count }
            })
            .collect();
        suggestions.sort_by(|a, b| b.count.cmp(&a.count));
        suggestions.truncate(15);

        Ok(suggestions)
    })
    .await;

    match result {
        Ok(Ok(suggestions)) => Json(suggestions).into_response(),
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

fn atc_class_description_for_code(atc_code: &str) -> &'static str {
    // Try most specific prefix first (longest match)
    let prefixes = [
        "B01AC", "B01A", "M01A", "N02B", "N02A", "C09A", "C09B", "C09C", "C09D",
        "C07", "C08", "C03C", "C03A", "C03", "C01A", "C01B", "C10A", "N06AB", "N06A",
        "A10", "H02", "L04", "L01", "N03", "N05A", "N05B", "N05C",
        "J01FA", "J01MA", "J01", "J02A", "J05A", "A02BC", "A02B", "G03A", "N07", "R03",
        "M04", "B03", "L02BA", "L02B", "V03AB", "M03A",
    ];
    for prefix in &prefixes {
        if atc_code.starts_with(prefix) {
            return atc_class_description(prefix);
        }
    }
    ""
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

        let class_keywords = load_class_keywords(&conn);

        let mut drugs_in_class: HashMap<String, usize> = HashMap::new();
        for (prefix, _) in &class_keywords {
            let count = drugs.iter().filter(|d| d.atc.starts_with(prefix.as_str())).count();
            drugs_in_class.insert(prefix.to_string(), count);
        }

        let mut total_pairs = 0u64;
        let mut classes = Vec::new();

        for (prefix, keywords) in &class_keywords {
            let n_in_class = *drugs_in_class.get(prefix.as_str()).unwrap_or(&0);
            if n_in_class == 0 { continue; }

            let mut mentioning_substances: HashSet<String> = HashSet::new();
            let mut best_keyword = String::new();
            let mut best_count = 0usize;

            for kw in keywords {
                let mut count = 0usize;
                for drug in &drugs {
                    if drug.atc.starts_with(prefix.as_str()) { continue; }
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
