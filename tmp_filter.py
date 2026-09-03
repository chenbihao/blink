import json
import sys

with open(r'd:\Projects\Coding\blink\corpus\manifest.json', 'r', encoding='utf-8') as f:
    manifest = json.load(f)

selected = ['B2_275', 'B2_335', 'B2_313']
manifest['samples'] = [s for s in manifest['samples'] if s['sample_id'] in selected]

with open(r'd:\Projects\Coding\blink\corpus\manifest_3sample.json', 'w', encoding='utf-8') as f:
    json.dump(manifest, f, ensure_ascii=False, indent=2)

print(f"Done. Sample count: {len(manifest['samples'])}")
for s in manifest['samples']:
    print(f"  {s['sample_id']}: {s['duration_s']}s - {s['reference_text'][:40]}...")
