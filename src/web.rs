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
