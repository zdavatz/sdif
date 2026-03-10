# SDIF - Swiss Drug Interaction Finder

A Rust tool that builds a searchable drug interactions SQLite database from the AmiKo Swiss drug database. It extracts interaction data from drug labels (Fachinformation) and enables basket-based interaction checking between drugs. Supports input by brand name or substance name.

![SDIF Stats](sdif_swiss_drug_interactions_finder_stats_12:16-10.03.2026.png)

## How it works

1. Downloads and reads the AmiKo full-text database (`amiko_db_full_idx_de.db`)
2. Extracts active substance names from ATC codes (German names), with fallback to the Zusammensetzung/Wirkstoffe HTML section when the ATC column lacks substance names
3. Parses the "Interaktionen" chapter plus interaction-relevant sentences from "Warnhinweise und Vorsichtsmassnahmen", "Kontraindikationen" and "Dosierung"
4. Uses Aho-Corasick multi-pattern matching to find substance mentions across all interaction texts
5. Scores severity of each interaction by scanning for German clinical keywords
6. Generates `interactions.db` with pre-computed interaction records

### Interaction detection strategies

- **Substance-level matching**: Direct lookup — does Drug A's interaction text mention Drug B's active substance?
- **ATC class-level matching**: Maps ATC code prefixes to German pharmacological class keywords (e.g. B01A → "Antikoagulantien", M01A → "Antiphlogistika") to catch class-level interactions like Ponstan (NSAID) ↔ Marcoumar (Vitamin-K-Antagonist)

## Build & Run

```bash
cargo build --release

# Download source DB and build interactions database (first time)
sdif build --download

# Rebuild without downloading (subsequent runs)
sdif build

# Check interactions between drugs (brand names or substance names)
sdif check Ponstan Marcoumar Aspirin
sdif check Phenprocoumon Navelbine

# Search interactions by clinical term
sdif search Prothrombinzeit
sdif search "QT-Verlängerung" -l 5
```

## CLI Usage

```
Swiss Drug Interaction Finder

Usage: sdif [COMMAND]

Commands:
  build   Build the interactions database (--download to fetch source DB)
  check   Check interactions between drugs in a basket
  search  Search interactions by clinical term
```

Running `sdif` without a subcommand defaults to `build`.

The `check` command accepts both brand names (Ponstan, Marcoumar) and substance names (Phenprocoumon, Mefenaminsäure). Substance names are resolved to their brand via the substance-brand map.

## Output

Generates `db/interactions.db` with the following tables:

- **drugs** — brand name, ATC code, ATC class, active substances, raw interaction text
- **interactions** — pre-computed substance-level interactions with context snippets, severity score and label
- **substance_brand_map** — maps substance names to brand names

### Stats

See the infographic at the top of this page — generated live from `db/interactions.db` via `python3 generate_stats.py`.

## Example: Ponstan + Marcoumar + Aspirin

```
$ sdif check Ponstan Marcoumar Aspirin

Basket contents:
  Ponstan® [M01AG01] -> mefenaminsäure
  Marcoumar® [B01AA04] -> phenprocoumon
  Aspirin® S [N02BA01] -> acetylsalicylsäure

INTERACTION [class-level]: Ponstan® <-> Marcoumar® (antikoagul) | Severity: # (Vorsicht)
  Mefenaminsäure verdrängt Warfarin aus dessen Proteinbindung,
  wodurch der gerinnungshemmende Effekt von Antikoagulantien
  vom Warfarin Typ verstärkt wird.

INTERACTION [substance match]: Ponstan® <-> Aspirin® S | Severity: - (Keine Einstufung)
  Via substance: acetylsalicylsäure
  Mefenaminsäure interferiert mit dem Thrombozytenaggregationseffekt
  von niedrig dosierter Acetylsalicylsäure (ASS)...

INTERACTION [class-level]: Aspirin® S <-> Ponstan® (entzündungshemm) | Severity: ### (Kontraindiziert)
  verstärkte Toxizität von Methotrexat...

INTERACTION [class-level]: Aspirin® S <-> Marcoumar® (antikoagul) | Severity: - (Keine Einstufung)
  Verstärkung der Wirkung von Antikoagulantien/Thrombolytika...

Severity levels: ### Kontraindiziert, ## Schwerwiegend, # Vorsicht, - Keine Einstufung
```

## Example: Search by clinical term

```
$ sdif search "QT-Verlängerung" -l 3

Found 13 interactions matching "QT-Verlängerung" (showing top 3):

Clarithromycin Sandoz® <-> domperidon (Motilium®) | Severity: ### (Kontraindiziert)
  ...was zu QT-Verlängerung und Arrhythmien einschliesslich
  ventrikulärer Tachykardie, Kammerflimmern und Torsades de Pointes führen kann.
```
