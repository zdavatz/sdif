# CLAUDE.md - SDIF (Swiss Drug Interaction Finder)

## Overview
Rust tool that builds a searchable drug interactions SQLite database from the AmiKo Swiss drug database. Extracts interaction data from Fachinformation HTML and enables basket-based interaction checking.

## Build & Run
```bash
# Download source DB (one-time)
mkdir -p db && cd db
curl -L -o amiko_db_full_idx_de.zip "http://pillbox.oddb.org/amiko_db_full_idx_de.zip"
unzip -o amiko_db_full_idx_de.zip && cd ..

# Build
cargo build --release

# Build interactions DB
sdif build

# Check drug interactions
sdif check Ponstan Marcoumar Aspirin
```

## Architecture
- **Source**: `src/main.rs` (single-file for now)
- **Input**: `db/amiko_db_full_idx_de.db` — AmiKo full-text DB with 4,564 drug entries
- **Output**: `db/interactions.db` — pre-computed interactions DB

### Key data flow
1. Parse ATC column (`"M01AG01;Mefenaminsäure"`) for German substance names
2. Extract "Interaktionen" HTML section from each drug's `content` column
3. Aho-Corasick multi-pattern match all known substances against interaction texts
4. Store substance-level interactions with context snippets

### Interaction detection
- **Substance-level**: Exact substance name match in interaction text (39,500 records)
- **ATC class-level**: Maps ~40 ATC prefixes to German class keywords for basket checks
  - e.g. B01A → "antikoagul", "warfarin"; M01A → "antiphlogistika", "nsar"
  - Covers: anticoagulants, NSAIDs, opioids, ACE inhibitors, sartans, beta-blockers, Ca-channel blockers, diuretics, cardiac glycosides, antiarrhythmics, statins, SSRIs/SNRIs, antidiabetics, corticosteroids, immunosuppressants, antineoplastics, antiepiletics, antipsychotics, anxiolytics, antibiotics (macrolides, fluoroquinolones), antimycotics, antivirals, PPIs, contraceptives, bronchodilators, gout agents, iron supplements

### Severity scoring
- Keyword-based scoring of interaction descriptions (German text)
- 3 = **Kontraindiziert** (`###`): "kontraindiziert", "darf nicht", "nicht angewendet werden"
- 2 = **Schwerwiegend** (`##`): "erhöhtes risiko", "lebensbedrohlich", "toxizität", "blutungsrisiko", "serotoninsyndrom"
- 1 = **Vorsicht** (`#`): "vorsicht", "überwach", "dosisanpassung", "verstärkt", "plasmaspiegel"
- 0 = **Keine Einstufung** (`-`): no severity keywords found

## Database schema (interactions.db)
- `drugs` (id, brand_name, atc_code, atc_class, active_substances, interactions_text)
- `interactions` (drug_brand, drug_substance, interacting_substance, interacting_brands, description, severity_score, severity_label)
- `substance_brand_map` (substance, brand_name)

## CLI
- `sdif build` — (re)build interactions.db from AmiKo source (default when no subcommand)
- `sdif check <drug1> <drug2> ...` — check basket of brand-name drugs for interactions

## Dependencies
- `rusqlite` (bundled SQLite), `regex`, `aho-corasick`, `anyhow`, `serde`/`serde_json`, `clap`
