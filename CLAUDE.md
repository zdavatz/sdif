# CLAUDE.md - SDIF (Swiss Drug Interaction Finder)

## Overview
Rust tool that builds a searchable drug interactions SQLite database from the AmiKo Swiss drug database and EPha interaction data. Extracts interaction data from Fachinformation HTML and EPha curated ATC-pair interactions, enables basket-based interaction checking.

## Build & Run
```bash
cargo build --release

# Download source DB and build (first time)
sdif build --download

# Rebuild without downloading
sdif build

# Build and publish to pillbox.oddb.org
sdif build --publish

# Check drug interactions (brand names or substance names)
sdif check Ponstan Marcoumar Aspirin
sdif check Phenprocoumon Navelbine

# Search by clinical term
sdif search Prothrombinzeit
sdif search "QT-Verlängerung" -l 5
```

## Architecture
- **Source**: `src/main.rs` (single-file for now)
- **Input**: `db/amiko_db_full_idx_de.db` — AmiKo full-text DB with 4,564 drug entries
- **Input**: `csv/atc.csv` — WHO ATC classification (downloaded from pillbox.oddb.org), used to cross-check ATC codes
- **Input**: `csv/drug_interactions_csv_de.csv` — EPha curated drug interactions (downloaded from pillbox.oddb.org as zip), 15,920 ATC-pair interactions with risk classes A–X
- **Output**: `db/interactions.db` — pre-computed interactions DB

### Key data flow
1. Parse ATC column (`"M01AG01;Mefenaminsäure"`) for German substance names; fallback to Zusammensetzung/Wirkstoffe HTML section when ATC column has code but no substance name
2. Cross-check ATC codes against `csv/atc.csv` (all levels: 1-digit to 7-digit); log mismatches (e.g. reclassified L01XX→L01E codes)
3. Extract "Interaktionen" HTML section + interaction-relevant sentences from "Warnhinweise", "Kontraindikationen", "Dosierung"
4. Aho-Corasick multi-pattern match all known substances against interaction texts
5. Store substance-level interactions with best-severity context snippets

### Interaction detection
- **Substance-level**: Exact substance name match in interaction text (46,887 records, 17,504 unique substance pairs)
- **ATC class-level**: Maps ~40 ATC prefixes to German class keywords for basket checks
  - Keywords defined in `txt/keywords.txt` and stored in `class_keywords` table during build
  - e.g. B01A → "antikoagul", "warfarin"; M01A → "antiphlogistika", "nsar"
  - Covers: anticoagulants, NSAIDs, opioids, ACE inhibitors, sartans, beta-blockers, Ca-channel blockers, diuretics, cardiac glycosides, antiarrhythmics, statins, SSRIs/SNRIs, antidiabetics, corticosteroids, immunosuppressants, antineoplastics, antiepiletics, antipsychotics, anxiolytics, antibiotics (macrolides, fluoroquinolones), antimycotics, antivirals, PPIs, contraceptives, bronchodilators, gout agents, iron supplements, SERMs (L02BA), muscle relaxants (M03A), antidotes (V03AB)
- **CYP enzyme-level**: Detects CYP450-mediated interactions at query time via `cyp_rules` table
  - If Drug A's interaction text mentions a CYP enzyme and Drug B is a known inhibitor/inducer, flags the interaction
  - Covers: CYP3A4, CYP2D6, CYP2C9, CYP2C19, CYP1A2, CYP2C8, CYP2B6
  - Inhibitors/inducers mapped by ATC prefix (e.g. J05AE = HIV protease inhibitors) and substance name (e.g. ritonavir, rifampicin)
  - Basket lookup prefers drug entries with longest interaction text to avoid matching sparse formulations
- **EPha curated**: Professionally graded ATC-pair interactions from EPha.ch database (separate `epha_interactions` table)
  - 9,133 unique ATC pairs with 5-level risk classification (A/B/C/D/X)
  - Includes structured effect, mechanism, and recommended measures
  - Looked up by ATC code pair during basket check when `--epha` flag is used; clearly labeled as "EPha" source
  - Severity mapping: X→3, D→2, C→1, B→1, A→0

### Severity scoring
- Keyword-based scoring of interaction descriptions (German text)
- Context extraction scans **all occurrences** of a substance and picks the snippet with the highest severity
- 3 = **Kontraindiziert** (`###`): "kontraindiziert", "darf nicht", "nicht angewendet werden"
- 2 = **Schwerwiegend** (`##`): "erhöhtes risiko", "lebensbedrohlich", "toxizität", "nephrotoxisch", "hepatotoxisch", "niereninsuffizienz", "nierenfunktionsstörung", "abgeraten", "wird nicht empfohlen", "lymphoproliferation"
- 1 = **Vorsicht** (`#`): "vorsicht", "überwach", "dosisanpassung", "verstärkt", "plasmaspiegel", "subtherapeutisch", "therapieversagen"
- 0 = **Keine Einstufung** (`-`): no severity keywords found

## Database schema (interactions.db)
- `drugs` (id, brand_name, atc_code, atc_class, active_substances, interactions_text)
- `interactions` (drug_brand, drug_substance, interacting_substance, interacting_brands, description, severity_score, severity_label)
- `substance_brand_map` (substance, brand_name)
- `epha_interactions` (atc1, atc2, risk_class, risk_label, effect, mechanism, measures, title, severity_score) — EPha curated data, queried by ATC pair
- `class_keywords` (atc_prefix, keyword) — ATC class keywords for class-level interaction detection, populated from `txt/keywords.txt` during build
- `cyp_rules` (enzyme, text_pattern, role, atc_prefix, substance) — CYP450 inhibitor/inducer rules for enzyme-mediated interaction detection, populated during build

## CLI
- `sdif build [--download] [--publish]` — (re)build interactions.db; `--download` fetches AmiKo source DB + ATC CSV + EPha CSV first; `--publish` deploys interactions.db to pillbox.oddb.org via scp
- `sdif check <drug1> <drug2> ...` — check basket for interactions (accepts brand names or substance names)
- `sdif search <term> [-l N]` — search interaction descriptions by clinical term (e.g. Prothrombinzeit, QT-Verlängerung), sorted by severity, shows all by default
- `sdif class-interactions` — list all class-level interactions across all drug pairs, showing per-ATC-class stats (drugs in class, drugs mentioning class keywords, potential pairs)
- `sdif serve [-p PORT] [--epha]` — start Axum web server (default port 3000); `--epha` enables EPha curated interactions alongside Swissmedic FI results (with source badges)

## Stats image
```bash
python3 generate_stats.py
```
- Generates `sdif_swiss_drug_interactions_finder_stats_HHhMM-dd.mm.yyyy.png`
- Reads all values live from `db/interactions.db` (no hardcoded stats)
- Requires matplotlib; embedded in README.md
- After regenerating, update the filename reference in README.md

## Web UI
- `sdif serve` starts Axum web server, serves `static/index.html` via `include_str!`
- API: `/api/search-drugs` (with `?q=` or `?atc=`), `/api/check`, `/api/search-interactions`, `/api/suggest-terms`, `/api/class-interactions`
- Frontend: vanilla HTML/CSS/JS, no build step
- Features: autocomplete with keyboard nav (↑/↓/Enter), auto-check on basket change, severity badge right after drug pair title, severity-colored cards with explanations, FI quality hints for asymmetric severity pairs, sortable ATC class table, shareable URLs with ATC codes (`?tab=check&drugs=M01AG01-B01AA04`), HTML entity decoding in descriptions, basket/clear hidden when empty
- With `--epha`: source badges (Swissmedic FI / EPha) on interaction cards, EPha results in basket check and clinical search
- Clinical search type-ahead: suggests both single words and bigram phrases (e.g. "hormonale Kontrazeptivum"), preserves original capitalization from source text, UTF-8 safe char boundary handling for multi-byte characters (e.g. em-dash «–»)

## Dependencies
- `rusqlite` (bundled SQLite), `regex`, `aho-corasick`, `anyhow`, `serde`/`serde_json`, `clap`
- `axum`, `tokio`, `tower-http` (web server)
