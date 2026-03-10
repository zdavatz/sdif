# CLAUDE.md - SDIF (Swiss Drug Interaction Finder)

## Overview
Rust tool that builds a searchable drug interactions SQLite database from the AmiKo Swiss drug database. Extracts interaction data from Fachinformation HTML and enables basket-based interaction checking.

## Build & Run
```bash
cargo build --release

# Download source DB and build (first time)
sdif build --download

# Rebuild without downloading
sdif build

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
- **Output**: `db/interactions.db` — pre-computed interactions DB

### Key data flow
1. Parse ATC column (`"M01AG01;Mefenaminsäure"`) for German substance names; fallback to Zusammensetzung/Wirkstoffe HTML section when ATC column has code but no substance name
2. Extract "Interaktionen" HTML section + interaction-relevant sentences from "Warnhinweise", "Kontraindikationen", "Dosierung"
3. Aho-Corasick multi-pattern match all known substances against interaction texts
4. Store substance-level interactions with best-severity context snippets

### Interaction detection
- **Substance-level**: Exact substance name match in interaction text (57,301 records, 21,695 unique substance pairs)
- **ATC class-level**: Maps ~40 ATC prefixes to German class keywords for basket checks
  - e.g. B01A → "antikoagul", "warfarin"; M01A → "antiphlogistika", "nsar"
  - Covers: anticoagulants, NSAIDs, opioids, ACE inhibitors, sartans, beta-blockers, Ca-channel blockers, diuretics, cardiac glycosides, antiarrhythmics, statins, SSRIs/SNRIs, antidiabetics, corticosteroids, immunosuppressants, antineoplastics, antiepiletics, antipsychotics, anxiolytics, antibiotics (macrolides, fluoroquinolones), antimycotics, antivirals, PPIs, contraceptives, bronchodilators, gout agents, iron supplements, SERMs (L02BA), muscle relaxants (M03A), antidotes (V03AB)

### Severity scoring
- Keyword-based scoring of interaction descriptions (German text)
- Context extraction scans **all occurrences** of a substance and picks the snippet with the highest severity
- 3 = **Kontraindiziert** (`###`): "kontraindiziert", "darf nicht", "nicht angewendet werden"
- 2 = **Schwerwiegend** (`##`): "erhöhtes risiko", "lebensbedrohlich", "toxizität", "nephrotoxisch", "hepatotoxisch", "abgeraten", "wird nicht empfohlen", "lymphoproliferation"
- 1 = **Vorsicht** (`#`): "vorsicht", "überwach", "dosisanpassung", "verstärkt", "plasmaspiegel", "subtherapeutisch", "therapieversagen"
- 0 = **Keine Einstufung** (`-`): no severity keywords found

## Database schema (interactions.db)
- `drugs` (id, brand_name, atc_code, atc_class, active_substances, interactions_text)
- `interactions` (drug_brand, drug_substance, interacting_substance, interacting_brands, description, severity_score, severity_label)
- `substance_brand_map` (substance, brand_name)

## CLI
- `sdif build [--download]` — (re)build interactions.db; `--download` fetches AmiKo source DB first
- `sdif check <drug1> <drug2> ...` — check basket for interactions (accepts brand names or substance names)
- `sdif search <term> [-l N]` — search interaction descriptions by clinical term (e.g. Prothrombinzeit, QT-Verlängerung), sorted by severity, default limit 20

## Stats image
- `python3 generate_stats.py` — generates `sdif_swiss_drug_interactions_finder_stats_dd.mm.yyyy.png`
- Requires matplotlib; embedded in README.md

## Dependencies
- `rusqlite` (bundled SQLite), `regex`, `aho-corasick`, `anyhow`, `serde`/`serde_json`, `clap`
