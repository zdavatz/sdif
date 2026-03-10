#!/usr/bin/env python3
"""Generate stats PNG for SDIF project, reading live data from interactions.db."""

import sqlite3
import sys
from datetime import date
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import matplotlib.patches as mpatches
from matplotlib.gridspec import GridSpec

DB_PATH = 'db/interactions.db'

# --- Read stats from database ---
try:
    conn = sqlite3.connect(DB_PATH)
except Exception as e:
    print(f"Error opening {DB_PATH}: {e}", file=sys.stderr)
    sys.exit(1)

num_drugs = conn.execute("SELECT COUNT(*) FROM drugs").fetchone()[0]
num_substances = conn.execute("SELECT COUNT(DISTINCT substance) FROM substance_brand_map").fetchone()[0]
num_interactions = conn.execute("SELECT COUNT(*) FROM interactions").fetchone()[0]
num_unique_pairs = conn.execute(
    "SELECT COUNT(*) FROM ("
    "  SELECT DISTINCT CASE WHEN drug_substance <= interacting_substance"
    "    THEN drug_substance ELSE interacting_substance END AS a,"
    "    CASE WHEN drug_substance <= interacting_substance"
    "    THEN interacting_substance ELSE drug_substance END AS b"
    "  FROM interactions"
    ")"
).fetchone()[0]

sev_counts = {}
for score, count in conn.execute(
    "SELECT severity_score, COUNT(*) FROM interactions GROUP BY severity_score"
):
    sev_counts[score] = count
conn.close()

sev_kontra = sev_counts.get(3, 0)
sev_schwer = sev_counts.get(2, 0)
sev_vorsicht = sev_counts.get(1, 0)
sev_keine = sev_counts.get(0, 0)
classified = num_interactions - sev_keine
pct_classified = int(classified / num_interactions * 100) if num_interactions else 0

# --- Build chart ---
fig = plt.figure(figsize=(14, 8), facecolor='#ffffff')
gs = GridSpec(2, 2, figure=fig, hspace=0.35, wspace=0.3,
             left=0.12, right=0.95, top=0.88, bottom=0.08)

today = date.today().strftime('%d.%m.%Y')
title_color = '#222222'
text_color = '#444444'
accent = '#0277bd'

fig.suptitle('SDIF - Swiss Drug Interaction Finder',
             fontsize=22, fontweight='bold', color=accent, y=0.96)
fig.text(0.95, 0.02, today, ha='right', fontsize=10, color=text_color)

# --- Top left: Key metrics ---
ax1 = fig.add_subplot(gs[0, 0])
ax1.set_facecolor('#ffffff')
ax1.axis('off')

metrics = [
    (f'{num_drugs:,}', 'Drugs parsed'),
    (f'{num_substances:,}', 'Unique substances'),
    (f'{num_interactions:,}', 'Interaction records'),
    (f'{num_unique_pairs:,}', 'Unique substance pairs'),
    ('~40', 'ATC class mappings'),
]

for i, (value, label) in enumerate(metrics):
    y = 0.88 - i * 0.19
    ax1.text(0.05, y, value, fontsize=22, fontweight='bold',
             color=accent, transform=ax1.transAxes, va='center')
    ax1.text(0.45, y, label, fontsize=13, color=text_color,
             transform=ax1.transAxes, va='center')

ax1.text(0.05, 1.05, 'Key Metrics', fontsize=14, fontweight='bold',
         color=title_color, transform=ax1.transAxes)

# --- Top right: Severity donut chart ---
ax2 = fig.add_subplot(gs[0, 1])
ax2.set_facecolor('#ffffff')

severity_labels = ['Kontraindiziert', 'Schwerwiegend', 'Vorsicht', 'Keine Einstufung']
severity_values = [sev_kontra, sev_schwer, sev_vorsicht, sev_keine]
severity_colors = ['#e53935', '#ff9800', '#fdd835', '#b0bec5']

wedges, texts, autotexts = ax2.pie(
    severity_values, labels=None, autopct='%1.0f%%',
    colors=severity_colors, startangle=90,
    pctdistance=0.78, wedgeprops=dict(width=0.45, edgecolor='#ffffff', linewidth=2)
)
for t in autotexts:
    t.set_fontsize(11)
    t.set_fontweight('bold')
    t.set_color('#ffffff')

# Center text
ax2.text(0, 0, f'{pct_classified}%\nclassified', ha='center', va='center',
         fontsize=13, fontweight='bold', color=accent)

ax2.set_title('Severity Distribution', fontsize=14, fontweight='bold',
              color=title_color, pad=12)

legend = ax2.legend(
    [mpatches.Patch(facecolor=c, edgecolor='#ffffff') for c in severity_colors],
    [f'{l}  ({v:,})' for l, v in zip(severity_labels, severity_values)],
    loc='lower center', bbox_to_anchor=(0.5, -0.15),
    ncol=2, fontsize=10, frameon=False,
)
for t in legend.get_texts():
    t.set_color(text_color)

# --- Bottom: Severity bar chart ---
ax3 = fig.add_subplot(gs[1, :])
ax3.set_facecolor('#ffffff')

severity_markers = ['###', '##', '#', '-']

bar_positions = [i * 1.4 for i in range(len(severity_labels))]
bars = ax3.barh(bar_positions, severity_values[::-1],
                color=severity_colors[::-1], edgecolor='#ffffff', height=0.7)

max_val = max(severity_values)
for i, (bar, val) in enumerate(zip(bars, severity_values[::-1])):
    label = severity_labels[::-1][i]
    marker = severity_markers[::-1][i]
    y_center = bar.get_y() + bar.get_height()/2
    y_top = bar.get_y() + bar.get_height() + 0.05
    # Label and marker above the bar
    ax3.text(0, y_top, f'{marker}  {label}', va='bottom', ha='left',
             fontsize=10, fontweight='bold', color=severity_colors[::-1][i])
    # Value inside the bar
    if bar.get_width() > max_val * 0.08:
        ax3.text(bar.get_width() * 0.5, y_center, f'{val:,}',
                 va='center', ha='center',
                 fontsize=11, fontweight='bold', color='#ffffff')
    else:
        ax3.text(bar.get_width() + 300, y_center, f'{val:,}',
                 va='center', fontsize=11, fontweight='bold', color=text_color)

ax3.set_xlim(0, max(severity_values) * 1.15)
ax3.set_title('Interaction Records by Severity Level', fontsize=14,
              fontweight='bold', color=title_color, pad=12)
ax3.set_yticks([])
ax3.spines['top'].set_visible(False)
ax3.spines['right'].set_visible(False)
ax3.spines['bottom'].set_color('#ccc')
ax3.spines['left'].set_visible(False)
ax3.xaxis.set_visible(False)

output = f'sdif_swiss_drug_interactions_finder_stats_{today}.png'
plt.savefig(output, dpi=150, facecolor=fig.get_facecolor())
plt.close()
print(f'Saved {output}')
