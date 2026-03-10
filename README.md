# SDIF - Swiss Drug Interaction Finder

A Rust tool that builds a searchable drug interactions SQLite database from the AmiKo Swiss drug database. It extracts interaction data from drug labels (Fachinformation) and enables basket-based interaction checking between brand-name drugs.

## How it works

1. Downloads and reads the AmiKo full-text database (`amiko_db_full_idx_de.db`)
2. Extracts active substance names from ATC codes (German names)
3. Parses the "Interaktionen" chapter from each drug's HTML content
4. Uses Aho-Corasick multi-pattern matching to find substance mentions across all interaction texts
5. Generates `interactions.db` with pre-computed interaction records

### Interaction detection strategies

- **Substance-level matching**: Direct lookup — does Drug A's interaction text mention Drug B's active substance?
- **ATC class-level matching**: Maps ATC code prefixes to German pharmacological class keywords (e.g. B01A → "Antikoagulantien", M01A → "Antiphlogistika") to catch class-level interactions like Ponstan (NSAID) ↔ Marcoumar (Vitamin-K-Antagonist)

## Setup

```bash
# Download the source database
mkdir -p db
curl -L -o db/amiko_db_full_idx_de.zip "http://pillbox.oddb.org/amiko_db_full_idx_de.zip"
unzip -o db/amiko_db_full_idx_de.zip -d db/
```

## Build & Run

```bash
cargo build --release

# Build the interactions database
sdif build

# Check interactions between drugs
sdif check Ponstan Marcoumar Aspirin
```

## CLI Usage

```
Swiss Drug Interaction Finder

Usage: sdif [COMMAND]

Commands:
  build  Build the interactions database from the AmiKo source DB
  check  Check interactions between drugs in a basket
```

Running `sdif` without a subcommand defaults to `build`.

## Output

Generates `db/interactions.db` with the following tables:

- **drugs** — brand name, ATC code, ATC class, active substances, raw interaction text
- **interactions** — pre-computed substance-level interactions with context snippets
- **substance_brand_map** — maps substance names to brand names

### Stats (as of March 2026)

- 3,983 drugs parsed
- 1,230 unique substances
- 39,500 interaction records
- ~40 ATC drug class keyword mappings

## Example: Ponstan + Marcoumar + Aspirin

```
$ sdif check Ponstan Marcoumar Aspirin

Basket contents:
  Ponstan® [M01AG01] -> mefenaminsäure
  Marcoumar® [B01AA04] -> phenprocoumon
  Aspirin® S [N02BA01] -> acetylsalicylsäure

INTERACTION [class-level]: Ponstan® <-> Marcoumar® (antikoagul)
  Mefenaminsäure verdrängt Warfarin aus dessen Proteinbindung,
  wodurch der gerinnungshemmende Effekt von Antikoagulantien
  vom Warfarin Typ verstärkt wird.

INTERACTION [substance match]: Ponstan® <-> Aspirin® S
  Via substance: acetylsalicylsäure
  Mefenaminsäure interferiert mit dem Thrombozytenaggregationseffekt
  von niedrig dosierter Acetylsalicylsäure (ASS)...

INTERACTION [class-level]: Aspirin® S <-> Marcoumar® (antikoagul)
  Verstärkung der Wirkung von Antikoagulantien/Thrombolytika...
```
