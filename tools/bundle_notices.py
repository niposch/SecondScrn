"""Collect supplemental upstream and NuGet licence notices for a release build."""
import concurrent.futures
import json
import pathlib
import urllib.request
import xml.etree.ElementTree as ET

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCES = {
 'MPL-2.0 (UniFFI 0.31.2)': 'https://raw.githubusercontent.com/mozilla/uniffi-rs/v0.31.2/LICENSE',
 'MIT (valuable)': 'https://raw.githubusercontent.com/tokio-rs/valuable/master/LICENSE',
 'MIT (asn1-rs)': 'https://raw.githubusercontent.com/rusticata/asn1-rs/master/LICENSE-MIT',
 'MIT (jni-sys)': 'https://raw.githubusercontent.com/jni-rs/jni-sys/master/LICENSE-MIT',
 'MIT (ndk)': 'https://raw.githubusercontent.com/rust-mobile/ndk/master/LICENSE-MIT',
 'Apache-2.0': 'https://www.apache.org/licenses/LICENSE-2.0.txt',
 'MIT': 'https://licenses.nuget.org/MIT',
}

def fetch(item):
    label, url = item
    try:
        with urllib.request.urlopen(url, timeout=25) as response:
            return label, url, response.read().decode('utf-8-sig')
    except Exception as e:
        return label, url, None

texts = ['Supplemental upstream licence texts', '']
failed = []
for label, url, content in concurrent.futures.ThreadPoolExecutor(max_workers=8).map(fetch, SOURCES.items()):
    if content is None:
        failed.append(label)
        continue
    texts += ['=' * 78, label, 'Source: ' + url, '', content, '']
# str0m split crates share the top-level licence.
texts += ['=' * 78, 'str0m, str0m-proto, str0m-rust-crypto, is',
          (ROOT / 'vendor/str0m/LICENSE-MIT.txt').read_text(encoding='utf-8'), '']
(ROOT / 'licenses/UPSTREAM-NOTICES.txt').write_text('\n'.join(texts), encoding='utf-8')

assets = json.loads((ROOT / 'apps/windows/SecondScrn.App/obj/project.assets.json').read_text(encoding='utf-8-sig'))
notices = ['Windows NuGet dependency notices', 'Includes runtime and build dependencies.', '']
rows = ['# Resolved Windows packages', '', 'Generated from the restored NuGet assets; includes build tools.', '',
        '| Package | Version | Licence declaration |', '|---|---|---|']
for key, value in sorted(assets['libraries'].items()):
    if value.get('type') != 'package':
        continue
    package_dir = None
    for folder in assets['packageFolders']:
        candidate = pathlib.Path(folder) / value['path']
        if candidate.is_dir():
            package_dir = candidate
            break
    if not package_dir:
        raise RuntimeError('Package not restored: ' + key)
    nuspec = next(package_dir.glob('*.nuspec'))
    doc = ET.parse(nuspec)
    data = {e.tag.split('}')[-1]: (e.text or '') for e in doc.iter()}
    name, version = key.split('/')
    rows.append(f"| {name} | {version} | {data.get('license', data.get('licenseUrl', 'See package'))} |")
    notices += ['=' * 78, key, 'Authors: ' + data.get('authors', ''),
                'Copyright: ' + data.get('copyright', ''),
                'Licence: ' + data.get('license', ''),
                'Project: ' + data.get('projectUrl', ''), '']
    found = set()
    for f in package_dir.rglob('*'):
        if not f.is_file():
            continue
        if f.name.lower().startswith(('license', 'licence', 'notice', 'thirdpartynotices', 'copyright')) and f.stat().st_size < 2_000_000:
            content = f.read_text(encoding='utf-8', errors='replace')
            if content not in found:
                notices += [f'--- {f.relative_to(package_dir).as_posix()} ---', content, '']
                found.add(content)
(ROOT / 'licenses/WINDOWS-NOTICES.txt').write_text('\n'.join(notices), encoding='utf-8')
(ROOT / 'docs/windows-dependencies.md').write_text('\n'.join(rows) + '\n', encoding='utf-8')
print('Collected Windows package notices. Failed upstream downloads:', ', '.join(failed) or 'none')
if failed:
    raise SystemExit(1)
